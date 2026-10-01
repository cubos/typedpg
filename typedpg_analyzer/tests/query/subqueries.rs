//! Subqueries: in FROM, scalar, IN / NOT IN, EXISTS / NOT EXISTS,
//! ARRAY(SELECT …).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            name  TEXT NOT NULL,
            age   INT
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

// ── Subquery in FROM ─────────────────────────────────────────────────────────

#[test]
fn subquery_in_from() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT sub.name, sub.age \
             FROM (SELECT name, age FROM users WHERE age IS NOT NULL) sub",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text()), cn("age", int4())]);
}

#[test]
fn complex_subquery_in_from() {
    let db = setup();
    let sql = "SELECT sub.user_name, sub.post_count \
               FROM ( \
                   SELECT u.name as user_name, COUNT(*) as post_count \
                   FROM users u \
                   INNER JOIN posts p ON p.user_id = u.id \
                   GROUP BY u.name \
               ) sub";
    let info = db.analyze(sql).unwrap();
    // user_name from NOT NULL column → NOT NULL through subquery.
    assert!(!col(&info, "user_name").nullable);
    // post_count is COUNT(*) → NOT NULL.
    assert!(!col(&info, "post_count").nullable);
}

// ── IN subquery ──────────────────────────────────────────────────────────────

#[test]
fn in_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users \
             WHERE id IN (SELECT user_id FROM posts)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn in_subquery_with_wrong_arity_rejected() {
    let db = setup();
    // PG: `subquery has too many columns`. A single-column LHS can't match
    // a multi-column subquery.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE id IN (SELECT id, name FROM users)"),
        AnalyzeError::Invalid(_),
        concat!(
            "subquery has too many columns (subquery has 2, lhs has 1)\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE id IN (SELECT id, name FROM users)\n",
            "  ·                               ──\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn not_in_subquery() {
    let db = setup();
    // NOT IN is a semi-anti-join — doesn't affect the outer row shape.
    let s = db
        .analyze(
            "SELECT id FROM users \
             WHERE id NOT IN (SELECT user_id FROM posts)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── ANY / ALL (subquery) ─────────────────────────────────────────────────────

#[test]
fn any_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id FROM users \
             WHERE id = ANY(SELECT user_id FROM posts)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn all_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id FROM users \
             WHERE age < ALL(SELECT rating FROM comments)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── Correlated scalar subquery ───────────────────────────────────────────────

#[test]
fn correlated_scalar_subquery_in_select_list() {
    let db = setup();
    // Inner subquery references outer `t.id` — the analyzer must thread the
    // outer scope into the subselect.
    let s = db
        .analyze(
            "SELECT id, (SELECT title FROM posts p WHERE p.user_id = u.id LIMIT 1) AS first_title \
             FROM users u",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("first_title", text())]);
}

// ── EXISTS with SELECT * ─────────────────────────────────────────────────────

#[test]
fn exists_with_select_star_accepts() {
    let db = setup();
    // EXISTS ignores the projected columns, so `SELECT *` inside is fine.
    let s = db
        .analyze("SELECT id FROM users WHERE EXISTS(SELECT * FROM posts)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── NOT EXISTS ───────────────────────────────────────────────────────────────

#[test]
fn not_exists_subquery() {
    let db = setup();
    // NOT EXISTS, like EXISTS, returns a definite bool.
    let s = db
        .analyze(
            "SELECT u.name, \
                    NOT EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id) AS orphan \
             FROM users u",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("orphan", bool_ty())]);
}

// ── EXISTS subquery ──────────────────────────────────────────────────────────

#[test]
fn exists_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users u \
             WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn complex_exists_subquery() {
    let db = setup();
    let sql = "SELECT u.name, EXISTS(SELECT 1 FROM posts p WHERE p.user_id = u.id) as has_posts \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // EXISTS always returns bool, never NULL.
    assert_cols(&info, vec![c("name", text()), c("has_posts", bool_ty())]);
}

// ── Scalar subqueries (always nullable unless aggregate without GROUP BY) ────

#[test]
fn complex_scalar_subquery_always_nullable() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_post \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // Scalar subquery is always nullable (zero rows → NULL).
    assert!(col(&info, "first_post").nullable);
}

#[test]
fn subquery_count_star_not_null() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // Aggregate without GROUP BY → guaranteed 1 row, COUNT is NOT NULL.
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_count_plus_one_not_null() {
    let db = setup();
    // COUNT(*) + 1 wraps the aggregate in an AExpr — must still detect it.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) + 1 FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_count_cast_not_null() {
    let db = setup();
    // COUNT(*)::int wraps aggregate in TypeCast.
    let sql = "SELECT (SELECT COUNT(*)::int FROM posts) as cnt FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_coalesce_sum_not_null() {
    let db = setup();
    // COALESCE(SUM(rating), 0) — aggregate detected through COALESCE.
    // SUM is nullable (empty group), but COALESCE with literal → NOT NULL.
    // Also: aggregate without GROUP BY → guaranteed 1 row.
    let sql = "SELECT (SELECT COALESCE(SUM(rating), 0) FROM comments) as total";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "total").nullable);
}

#[test]
fn subquery_sum_nullable() {
    let db = setup();
    // SUM without COALESCE: aggregate != COUNT → nullable result.
    // Even though guaranteed 1 row, SUM itself returns NULL for empty input.
    let sql = "SELECT (SELECT SUM(rating) FROM comments) as total";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "total").nullable);
}

#[test]
fn subquery_with_group_by_still_nullable() {
    let db = setup();
    // COUNT(*) with GROUP BY: subquery may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id GROUP BY p.user_id) as cnt \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "cnt").nullable);
}

#[test]
fn subquery_non_aggregate_still_nullable() {
    let db = setup();
    // Non-aggregate scalar subquery: may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_title \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "first_title").nullable);
}

#[test]
fn subquery_case_wrapping_count_not_null() {
    let db = setup();
    // CASE WHEN ... THEN COUNT(*) ELSE 0 END — aggregate inside CASE with ELSE.
    let sql = "SELECT (SELECT CASE WHEN true THEN COUNT(*) ELSE 0 END FROM posts) as cnt";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_deeply_nested_subquery() {
    let db = setup();
    let sql = "SELECT * FROM ( \
                   SELECT * FROM ( \
                       SELECT id, name, age FROM users \
                   ) inner_sq \
               ) outer_sq";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn stress_subquery_with_left_join_inside() {
    let db = setup();
    // Subquery does LEFT JOIN, outer SELECT sees nullable cols.
    let sql = "SELECT sq.name, sq.title FROM ( \
                   SELECT u.name, p.title \
                   FROM users u \
                   LEFT JOIN posts p ON p.user_id = u.id \
               ) sq";
    let info = db.analyze(sql).unwrap();
    // title is nullable because of LEFT JOIN inside subquery; name stays NOT NULL.
    assert_cols(&info, vec![c("name", text()), cn("title", text())]);
}

#[test]
fn stress_subquery_computed_columns() {
    let db = setup();
    let sql = "SELECT sq.cnt, sq.max_age FROM ( \
                   SELECT COUNT(*) as cnt, MAX(age) as max_age FROM users \
               ) sq";
    let info = db.analyze(sql).unwrap();
    // COUNT is NOT NULL, MAX is nullable.
    assert_cols(&info, vec![c("cnt", int8()), cn("max_age", int4())]);
}

#[test]
fn stress_aggregate_subquery_in_select() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as post_count \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // Aggregate without GROUP BY → exactly 1 row, and COUNT is NOT NULL →
    // scalar subquery result is NOT NULL.
    assert_cols(&info, vec![c("name", text()), c("post_count", int8())]);
}

// ── Torture ──────────────────────────────────────────────────────────────────

// ── Multi-column IN (a, b) IN (SELECT …) ─────────────────────────────────────

#[test]
fn multi_column_in_subquery() {
    let db = setup();
    // PG: `(user_id, post_id) IN (SELECT a, b FROM …)` — the LHS row must
    // align with the subquery's column count, and the analyzer must not
    // collapse it to a single-column comparison.
    let s = db
        .analyze(
            "SELECT id FROM comments \
             WHERE (post_id, author_name) IN ( \
                 SELECT id, title FROM posts \
             )",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn multi_column_in_subquery_arity_mismatch_rejected() {
    let db = setup();
    // PG: `subquery has too few columns`. LHS has 2, subquery emits 1.
    assert_analyze_err!(
        db.analyze(
            "SELECT id FROM comments \
             WHERE (post_id, author_name) IN (SELECT id FROM posts)"
        ),
        AnalyzeError::Invalid(_),
        concat!(
            "subquery has too few columns (subquery has 1, lhs has 2)\n",
            "  ╭────\n",
            "1 │ SELECT id FROM comments WHERE (post_id, author_name) IN (SELECT id FROM posts)\n",
            "  ·                                                      ──\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn multi_column_not_in_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id FROM comments \
             WHERE (post_id, author_name) NOT IN ( \
                 SELECT id, title FROM posts \
             )",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn torture_union_in_subquery_in_from() {
    let db = setup();
    let sql = "SELECT sq.val FROM ( \
                   SELECT name as val FROM users \
                   UNION ALL \
                   SELECT title as val FROM posts \
               ) sq";
    let info = db.analyze(sql).unwrap();
    // Both NOT NULL → union NOT NULL → subquery NOT NULL.
    assert_cols(&info, vec![c("val", text())]);
}

// ── Implicit column name of a scalar subquery (PG FigureColname) ─────────────

#[test]
fn scalar_subquery_named_after_its_output_column() {
    // PG names an unaliased `(SELECT count(*) …)` after the subquery's single
    // output column (`count`), not `?column?`.
    let db = setup();
    let s = db.analyze("SELECT (SELECT count(*) FROM users)").unwrap();
    assert_eq!(col(&s, "count").name, "count");
}

#[test]
fn scalar_subquery_without_named_output_is_question_column() {
    // An unnamed output expression leaves the subquery as `?column?`.
    let db = setup();
    let s = db
        .analyze("SELECT (SELECT id + 1 FROM users LIMIT 1)")
        .unwrap();
    assert_eq!(s.columns[0].name, "?column?");
}

// ── IN/ANY/ALL subquery: comparison operator must resolve ────────────────────

#[test]
fn in_subquery_with_incompatible_types_rejected() {
    // `bigint IN (SELECT text …)` has no `=` operator. PG resolves the IN
    // comparison the same way as a plain `a = b` and rejects it.
    // PG: `operator does not exist: bigint = text`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM posts WHERE user_id IN (SELECT title FROM posts)"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: bigint = text\n",
            "  ╭────\n",
            "1 │ SELECT id FROM posts WHERE user_id IN (SELECT title FROM posts)\n",
            "  ·                                    ─┬\n",
            "  ·                                     ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `bigint = bigint` exists: cast the right operand to bigint (`expr::bigint`)\n",
        ),
    );
}

#[test]
fn in_subquery_with_castable_types_accepted() {
    // `integer IN (SELECT bigint …)` is fine — PG's cross-type `int4 = int8`
    // operator resolves it. Guards against over-rejecting valid IN-subqueries.
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age IN (SELECT id FROM users)")
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
}

// ── LATERAL scope semantics (resolution precedence, ambiguity, `*`) ─────────

#[test]
fn lateral_star_excludes_lateral_sources() {
    // Inside a LATERAL subquery, `SELECT *` expands only the subquery's own
    // FROM — the laterally-visible outer aliases are reachable by name but
    // are not part of the star.
    let db = setup();
    let s = db
        .analyze("SELECT l.* FROM users, LATERAL (SELECT * FROM posts) AS l")
        .unwrap();
    assert_eq!(
        s.columns.len(),
        db.analyze("SELECT * FROM posts").unwrap().columns.len(),
        "lateral star must expand only the inner FROM"
    );
}

#[test]
fn lateral_inner_from_shadows_lateral_ref() {
    // `id` exists in both the inner FROM (posts) and the lateral outer
    // (users) — PG resolves to the inner one with no ambiguity.
    let db = setup();
    db.analyze("SELECT 1 FROM users, LATERAL (SELECT id + 1 AS q FROM posts) AS l")
        .unwrap();
}

#[test]
fn lateral_two_same_level_sources_are_ambiguous() {
    // Two lateral sources at the same level sharing a column name *are*
    // ambiguous when referenced from the subquery.
    let db = setup();
    let err = db
        .analyze(
            "SELECT 1 FROM users u1, users u2, LATERAL (SELECT name || 'x' AS q FROM posts) AS l",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("column reference \"name\" is ambiguous"),
        "got: {err}"
    );
}

#[test]
fn name_repeated_inside_one_from_entry_is_ambiguous() {
    // PG's scanRTEForColumn: two columns of one entry sharing the name make
    // a reference to it ambiguous, qualified or not; `*` still lists both.
    let db = setup();
    for sql in [
        "SELECT s.a FROM (SELECT 1 a, 2 a) s",
        "SELECT a FROM (SELECT 1 a, 2 a) s",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::AmbiguousColumn(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("column reference \"a\" is ambiguous"),
            "{sql}: {err}"
        );
    }
    let s = db.analyze("SELECT * FROM (SELECT 1 a, 2 a) s").unwrap();
    assert_eq!(s.columns.len(), 2);
}

// ── ARRAY(SELECT …) over array / unknown columns, one-column rule ───────────

#[test]
fn array_sublink_over_array_and_unknown_columns() {
    // PG (transformSubLink): ARRAY(SELECT arr) over an array column is the
    // same array type (a multi-dimensional result), and an unknown-typed
    // target is resolved to text first (resolveTargetListUnknowns).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (arr int[] NOT NULL, tarr text[] NOT NULL);")
        .unwrap();
    let s = db
        .analyze(
            "SELECT ARRAY(SELECT arr FROM t) AS a, ARRAY(SELECT tarr FROM t) AS b, \
             ARRAY(SELECT NULL) AS c, (SELECT NULL) AS d, (SELECT 'x') AS e",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", array_of(int4())),
            c("b", array_of(text())),
            c("c", array_of(text())),
            cn("d", text()),
            // A subquery without FROM yields exactly one row.
            c("e", text()),
        ],
    );
    // The unknown target of a subquery is text, so comparing it to an int
    // fails exactly like PG: `operator does not exist: text = integer`.
    let err = db.analyze("SELECT (SELECT NULL) = 1").unwrap_err();
    assert!(matches!(err, AnalyzeError::UndefinedOperator(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("operator does not exist: text = integer"),
        "{err}"
    );
    let s = db.analyze("SELECT ARRAY(SELECT $p) AS a").unwrap();
    assert_cols(&s, vec![c("a", array_of(text()))]);
    assert_params(&s, vec![p(text())]);
}

#[test]
fn scalar_and_array_sublinks_require_one_column() {
    let db = setup();
    for sql in [
        "SELECT (SELECT 1, 2)",
        "SELECT ARRAY(SELECT id, name FROM users) AS a",
        "SELECT id FROM users WHERE id = (SELECT 1, 2)",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::SyntaxError(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("subquery must return only one column"),
            "{sql}: {err}"
        );
    }
}

#[test]
fn qualified_reference_stops_at_nearest_entry_of_that_name() {
    // PG's `refnameNamespaceItem`: the sublink's own `u` hides the outer
    // `u`, even though only the outer one has a column `v`.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
         CREATE TABLE o (id int, z int);",
    )
    .unwrap();
    assert_err_prefix!(
        db.analyze("SELECT (SELECT u.v FROM o AS u LIMIT 1) FROM t AS u"),
        AnalyzeError::UndefinedColumn(_),
        "column u.v does not exist"
    );
}

/// A FROM subquery can't see its own level's FROM items (without LATERAL),
/// but it still sees the enclosing query levels as outer references.
#[test]
fn from_subquery_sees_enclosing_levels() {
    let db = setup();
    let s = db
        .analyze("SELECT (SELECT x FROM (SELECT u.age AS x) q) FROM users u")
        .unwrap();
    assert_cols(&s, vec![cn("x", int4())]);
    db.analyze(
        "SELECT * FROM users u WHERE EXISTS \
         (SELECT 1 FROM (SELECT p.id FROM posts p WHERE p.user_id = u.id) q)",
    )
    .unwrap();
    db.analyze("SELECT * FROM users u, LATERAL (SELECT * FROM (SELECT u.name) i) q")
        .unwrap();
}

/// A scalar subquery is NOT NULL only when it yields exactly one row with
/// a NOT NULL value: an aggregate query of its own level (not over an
/// outer query's columns) without GROUP BY / HAVING, or a query without
/// FROM / WHERE — and no LIMIT / OFFSET that can drop the row.
#[test]
fn scalar_subquery_is_not_null_only_when_it_yields_exactly_one_row() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE u (id int PRIMARY KEY, tid int NOT NULL, x text);",
    )
    .unwrap();
    let s = db
        .analyze(
            "SELECT (SELECT count(*) FROM u) a, (SELECT count(*) FROM u LIMIT 1) b, \
             (SELECT count(*) FROM u OFFSET 0) c, (SELECT 1) d, (SELECT t.name) e, \
             (SELECT count(*) FROM u HAVING false) f, (SELECT count(*) FROM u LIMIT 0) g, \
             (SELECT count(*) FROM u OFFSET 1) h, \
             (SELECT count(*) FROM u FETCH FIRST 0 ROWS ONLY) i, \
             (SELECT count(*) FROM u LIMIT $l) j, \
             (SELECT 1 WHERE false) l, (SELECT generate_series(1, count(*)::int) FROM u) m \
             FROM t",
        )
        .unwrap();
    let not_null: Vec<&str> = s
        .columns
        .iter()
        .filter(|c| !c.nullable)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(not_null, ["a", "b", "c", "d", "e"]);
    // `count(t.id)` is the outer query's aggregate: the subquery is a plain
    // one over `u`, empty when `u` is.
    let s = db
        .analyze("SELECT (SELECT count(t.id) FROM u) AS k FROM t GROUP BY t.id")
        .unwrap();
    assert_cols(&s, vec![cn("k", int8())]);
}

/// A sublink reaches every enclosing level, not only the nearest one; set
/// operation arms and VALUES rows are subqueries too.
#[test]
fn sublink_reaches_every_enclosing_level() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
         CREATE TABLE s (id int PRIMARY KEY, w int);",
    )
    .unwrap();
    let s = db
        .analyze(
            "SELECT (SELECT (SELECT t.id)) AS a, (SELECT (SELECT v FROM s LIMIT 1)) AS b FROM t",
        )
        .unwrap();
    assert_cols(&s, vec![c("a", int4()), cn("b", int4())]);
    // The nearest level's column hides an outer one of the same name.
    let s = db
        .analyze("SELECT (SELECT (SELECT id) FROM s LIMIT 1) AS a FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4())]);
    // Set operation arms and VALUES rows see outer columns, and name the
    // sublink after their first column.
    let s = db
        .analyze(
            "SELECT (SELECT t.id UNION SELECT 1 LIMIT 1), (SELECT sum(t.v) UNION SELECT 1 LIMIT 1), \
             (VALUES (t.id)) FROM t GROUP BY t.id",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("id", int4()), cn("sum", int8()), c("column1", int4())],
    );
    let s = db
        .analyze("SELECT * FROM t, LATERAL (SELECT t.v UNION SELECT 1) q")
        .unwrap();
    assert_eq!(s.columns.len(), 3);
    // A non-LATERAL subquery can't see its sibling FROM entries — nor can
    // its set operation arms or VALUES rows.
    for sql in [
        "SELECT * FROM t, (SELECT t.v UNION SELECT 1) q",
        "SELECT * FROM t, (VALUES (t.v)) q",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::UndefinedTable(_),
            "invalid reference to FROM-clause entry for table \"t\""
        );
    }
}

/// PG names a scalar subquery after its transformed first target entry —
/// a `*`'s column, a set operation's left arm's — at strength 2, so a cast
/// around it keeps that name.
#[test]
fn scalar_subquery_named_after_its_first_output_column() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT (SELECT * FROM (SELECT 1 AS a) q), \
             (SELECT * FROM (SELECT 1 AS b) q UNION SELECT 2), \
             (SELECT 1)::int, (SELECT count(*) FROM users)::int, \
             (SELECT * FROM (VALUES (1)) v)",
        )
        .unwrap();
    let names: Vec<&str> = s.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "?column?", "count", "column1"]);
}

/// A ROW compared with a subquery expands its `t.*` / `(expr).*` items
/// (transformExpressionList) before the column counts are matched.
#[test]
fn row_star_items_expand_in_row_sublink_comparisons() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, w text);
         CREATE TYPE pr AS (x int, y text);
         CREATE TABLE c (id int PRIMARY KEY, p pr);",
    )
    .unwrap();
    for sql in [
        "SELECT ROW(t.*) IN (SELECT id, a, w FROM t) FROM t",
        "SELECT ROW(t.*) = (SELECT id, a, w FROM t LIMIT 1) FROM t",
        "SELECT ROW(t.*) = ANY (SELECT id, a, w FROM t) FROM t",
        "SELECT ROW(t.*, 1) IN (SELECT id, a, w, 2 FROM t) FROM t",
        "SELECT ROW((c.p).*) IN (SELECT 1, 'x') FROM c",
        "SELECT ROW((c.p).*) = (SELECT 1, 'x') FROM c",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for sql in [
        "SELECT ROW(t.*) IN (SELECT id, a FROM t) FROM t",
        "SELECT ROW(t.*) IN (SELECT t2 FROM t t2) FROM t",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            err.to_string().starts_with("subquery has too few columns"),
            "{sql}: {err}"
        );
    }
}
