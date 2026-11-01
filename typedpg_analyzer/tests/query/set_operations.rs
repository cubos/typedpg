//! UNION, UNION ALL, INTERSECT, EXCEPT: column unification, nullability
//! propagation across branches, type coercion of mixed branches.

use crate::common::*;

#[test]
fn union_with_incompatible_concrete_types_rejected() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, s TEXT NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("SELECT id FROM t UNION SELECT s FROM t"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "UNION types bigint and text cannot be matched (column `id`)\n",
            "  ╭────\n",
            "1 │ SELECT id FROM t UNION SELECT s FROM t\n",
            "  ·                               ┬\n",
            "  ·                               ╰─ this is text\n",
            "  ╰────\n",
            "  help: cast both sides to a common type, e.g. `id::bigint`\n",
        ),
    );
}

#[test]
fn union_with_incompatible_unknown_literal_rejected() {
    // `'text'` is an untyped string literal: PG keeps it `unknown` in a
    // set-operation arm and coerces it to the other arm's integer, which
    // fails in the input function.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT 1 UNION SELECT 'text'"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"text\"",
    );
}

/// select_common_type keeps the first arm's type when two types of one
/// category don't coerce either way implicitly; coerce_to_common_type then
/// fails converting the other arm (42846), naming the operation.
#[test]
fn set_operation_type_mismatch_message() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TYPE cp AS (x int, y text); CREATE TYPE cq AS (x int, y text);")
        .unwrap();
    for (sql, msg) in [
        (
            "SELECT ARRAY[1] UNION SELECT ARRAY['a']",
            "UNION could not convert type text[] to integer[]",
        ),
        (
            "SELECT ARRAY['a'] UNION SELECT ARRAY[1]",
            "UNION could not convert type integer[] to text[]",
        ),
        (
            "SELECT int4range(1,2) UNION ALL SELECT int8range(1,2)",
            "UNION could not convert type int8range to int4range",
        ),
        (
            "SELECT 1::money UNION SELECT 1::int",
            "UNION could not convert type integer to money",
        ),
        (
            "SELECT 1::int UNION SELECT 1::bigint UNION SELECT 1::money",
            "UNION could not convert type money to bigint",
        ),
        (
            "SELECT '(1,a)'::cp INTERSECT SELECT '(1,a)'::cq",
            "INTERSECT could not convert type cq to cp",
        ),
        (
            "SELECT 1, ARRAY[1] EXCEPT SELECT 2, ARRAY[now()]",
            "EXCEPT could not convert type timestamp with time zone[] to integer[]",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::CannotCoerce(_), msg);
    }
    // An implicit cast between the element types converts the arrays.
    let s = db
        .analyze("SELECT ARRAY[1] UNION SELECT ARRAY[1::bigint]")
        .unwrap();
    assert_cols(&s, vec![c("array", array_of(int8()))]);
}

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            age  INT
         );
         CREATE TABLE posts (
            id      BIGINT PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL,
            body    TEXT
         );
         CREATE TABLE comments (
            id          BIGINT PRIMARY KEY,
            post_id     BIGINT NOT NULL,
            author_name TEXT NOT NULL,
            content     TEXT NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── UNION / UNION ALL / INTERSECT / EXCEPT type match ────────────────────────

#[test]
fn types_match_union_all() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users WHERE age > 20 \
             UNION ALL \
             SELECT id, name FROM users WHERE age <= 20",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn types_match_union_distinct() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT name FROM users \
             UNION \
             SELECT title FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn types_match_intersect() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT name FROM users \
             INTERSECT \
             SELECT title FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn types_match_except() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT name FROM users \
             EXCEPT \
             SELECT title FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn types_match_cte_union() {
    let db = setup();
    let s = db
        .analyze(
            "WITH all_names AS (\
               SELECT name FROM users \
               UNION ALL \
               SELECT author_name AS name FROM comments\
             ) \
             SELECT name FROM all_names",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

// ── Nullability propagation across branches ──────────────────────────────────

#[test]
fn union_nullable_branch() {
    let db = setup();
    // Nullable because body is nullable.
    let sql = "SELECT name as val FROM users UNION ALL SELECT body as val FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("val", text())]);
}

#[test]
fn union_all_not_null() {
    let db = setup();
    let sql = "SELECT name as val FROM users UNION ALL SELECT title as val FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("val", text())]);
}

// ── Stress / complex ─────────────────────────────────────────────────────────

#[test]
fn stress_union_with_null_literal_branch() {
    let db = setup();
    // One branch is a literal NULL → union should be nullable.
    let sql = "SELECT name as val FROM users \
               UNION ALL \
               SELECT NULL as val";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "val").nullable);
}

#[test]
fn stress_union_mixed_types() {
    let db = setup();
    // int + bigint → bigint (coercion).
    let sql = "SELECT age as num FROM users \
               UNION ALL \
               SELECT id as num FROM users";
    let info = db.analyze(sql).unwrap();
    // age is nullable → union is nullable (even though id is NOT NULL).
    assert_cols(&info, vec![cn("num", int8())]);
}

#[test]
fn complex_union_three_branches() {
    let db = setup();
    let sql = "SELECT name as label FROM users \
               UNION ALL \
               SELECT title as label FROM posts \
               UNION ALL \
               SELECT author_name as label FROM comments";
    let info = db.analyze(sql).unwrap();
    // All three NOT NULL → union NOT NULL.
    assert!(!col(&info, "label").nullable);
}

#[test]
fn union_varchar_first_branch_keeps_varchar() {
    // PG's `select_common_type` only switches the running candidate when the
    // implicit cast is one-way; varchar ↔ text casts exist in both
    // directions, so the *first* branch's type wins: varchar UNION text is
    // varchar (and text UNION varchar is text). Found by the differential
    // fuzzer — collapsing every string mix to text diverges from Describe.
    let db = setup();
    let s = db
        .analyze("SELECT name::varchar AS v FROM users UNION SELECT title FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("v", varchar())]);

    let s = db
        .analyze("SELECT name AS v FROM users UNION SELECT title::varchar FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("v", text())]);
}

#[test]
fn set_op_column_count_mismatch_uses_pg_wording() {
    let db = setup();
    let err = db
        .analyze("SELECT id FROM users UNION SELECT id, title FROM posts")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("each UNION query must have the same number of columns"),
        "got: {err}"
    );
    let err = db
        .analyze("SELECT id FROM users INTERSECT SELECT id, title FROM posts")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("each INTERSECT query must have the same number of columns"),
        "got: {err}"
    );
}

/// EXCEPT only emits left rows, and INTERSECT only left rows matched by a
/// right one (NULLs match each other): a column is NULL only when the left
/// arm's is — and for INTERSECT, the right arm's too.
#[test]
fn except_and_intersect_nullability_follows_the_left_arm() {
    let db = setup();
    let s = db
        .analyze("SELECT name AS a, age AS b FROM users EXCEPT SELECT NULL, 1")
        .unwrap();
    assert_cols(&s, vec![c("a", text()), cn("b", int4())]);
    let s = db
        .analyze("SELECT name AS a, age AS b, age AS c FROM users INTERSECT SELECT NULL, 1, age FROM users")
        .unwrap();
    assert_cols(&s, vec![c("a", text()), c("b", int4()), cn("c", int4())]);
    let s = db
        .analyze("SELECT name AS a FROM users UNION SELECT NULL")
        .unwrap();
    assert_cols(&s, vec![cn("a", text())]);
}

/// Asserts each query fails with `msg` (`Some`) or analyzes (`None`).
fn assert_outcomes(db: &PgCatalog, cases: &[(&str, Option<&str>)]) {
    for (sql, fails) in cases {
        match (db.analyze(sql), fails) {
            (Err(err), Some(msg)) => {
                assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}")
            }
            (Ok(_), None) => {}
            (other, _) => panic!("{sql}: {other:?}"),
        }
    }
}

/// PG 18 fetches the first row of a set operation's arms whatever the
/// plan: every plan for a UNION reads the first row of each arm (a
/// HashAggregate or Sort reads them all, a MergeAppend primes each child),
/// an Append runs a UNION ALL's arms in order, and EXCEPT and INTERSECT
/// read their outer input whole, then the inner one if the outer one had a
/// row (an EXCEPT's outer input is its left arm, an INTERSECT's the one
/// with fewer groups). A NULL cast to a NOT NULL domain there fails every
/// execution, under a LIMIT too.
#[test]
fn a_failing_first_row_of_a_set_operation_fails_every_execution() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE DOMAIN d AS int NOT NULL;
         CREATE TABLE t (a int);
         CREATE VIEW v AS SELECT NULL::d AS x, 1 AS y;",
    )]);
    let domain = Some("domain d does not allow null values");
    assert_outcomes(
        &db,
        &[
            ("SELECT 1 UNION SELECT NULL::d LIMIT 1", domain),
            ("SELECT NULL::d UNION SELECT 1 LIMIT 1", domain),
            ("SELECT 1 UNION SELECT NULL::d LIMIT 2", domain),
            ("SELECT NULL::d UNION ALL SELECT 1 LIMIT 1", domain),
            ("SELECT x FROM v UNION ALL SELECT 1 LIMIT 1", domain),
            (
                "SELECT NULL::d UNION ALL SELECT 1 UNION ALL SELECT 2 LIMIT 1",
                domain,
            ),
            (
                "SELECT 1 UNION ALL SELECT NULL::d UNION SELECT 2 LIMIT 1",
                domain,
            ),
            ("SELECT (SELECT NULL::d) UNION ALL SELECT 1 LIMIT 1", domain),
            (
                "SELECT 1 UNION ALL SELECT NULL::d ORDER BY 1 LIMIT 1",
                domain,
            ),
            (
                "SELECT a FROM (SELECT NULL::d AS a UNION SELECT 1 LIMIT 1) s",
                domain,
            ),
            ("VALUES (NULL::d), (1) LIMIT 1", domain),
            ("SELECT NULL::d EXCEPT SELECT 1 WHERE false", domain),
            (
                "(SELECT 1 UNION ALL SELECT NULL::d) EXCEPT SELECT 1 WHERE false",
                domain,
            ),
            ("SELECT NULL::d EXCEPT SELECT a FROM t", domain),
            ("SELECT 1 EXCEPT SELECT NULL::d LIMIT 1", domain),
            ("SELECT NULL::d INTERSECT SELECT 1 WHERE false", domain),
            ("SELECT 1 INTERSECT SELECT NULL::d", domain),
            ("SELECT NULL::d INTERSECT SELECT 1 LIMIT 1", domain),
            ("SELECT 1 INTERSECT SELECT NULL::d LIMIT 1", domain),
            ("SELECT NULL::d INTERSECT SELECT a FROM t", domain),
            (
                "SELECT NULL::d INTERSECT SELECT a FROM t WHERE false",
                domain,
            ),
            ("SELECT x FROM v INTERSECT SELECT 1 WHERE false", domain),
            // Past the first row, or an arm that may not be read.
            ("SELECT 1 UNION ALL SELECT NULL::d LIMIT 1", None),
            ("VALUES (1), (NULL::d) LIMIT 1", None),
            (
                "SELECT 1 FROM (VALUES (1), (NULL::d)) s(a) WHERE a = 1 LIMIT 1",
                None,
            ),
            ("SELECT 1 WHERE false INTERSECT SELECT NULL::d", None),
            ("SELECT 1 WHERE false EXCEPT SELECT NULL::d", None),
            ("SELECT a FROM t EXCEPT SELECT NULL::d", None),
            ("SELECT a FROM t WHERE false INTERSECT SELECT NULL::d", None),
            ("SELECT 1 WHERE false INTERSECT SELECT x FROM v", None),
            (
                "(SELECT 1 UNION ALL SELECT NULL::d) INTERSECT SELECT 1 WHERE false",
                None,
            ),
            // Each arm projects its whole row, but a WHERE above is pushed
            // down into the arms (subquery_push_qual) and filters it first.
            (
                "SELECT b FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s",
                domain,
            ),
            (
                "SELECT a FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s WHERE b = 2",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s WHERE b IS NULL",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION SELECT 1, 2) s WHERE b IS NULL",
                None,
            ),
        ],
    );
}

/// Where every plan reads a set operation's arms whole. A grouped leaf of
/// no FROM item is one group, estimated one row — an INTERSECT reads it
/// after its one-row left input. Sorting reads every row's keys before a
/// LIMIT returns the first, unless a WHERE equating a key to a constant
/// drops the Sort. A qualification is pushed down into the arms
/// only through columns every leaf yields the set operation's type of
/// (compare_tlist_datatypes), and not into an EXCEPT, a LIMIT or through
/// the whole row: it then runs over the projected rows. And a set
/// operation the planner doesn't flatten into an append relation (a UNION
/// ALL of mismatched leaves, any other) keeps every column of its arms, so
/// reading any row projects them whole.
#[test]
fn a_set_operation_read_whole_fails_every_execution() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE DOMAIN d AS int NOT NULL;
         CREATE TABLE t (a int);
         CREATE VIEW v AS SELECT NULL::d AS x, 1 AS y;",
    )]);
    let domain = Some("domain d does not allow null values");
    assert_outcomes(
        &db,
        &[
            ("SELECT NULL::d GROUP BY 1", domain),
            ("SELECT 1 INTERSECT SELECT NULL::d GROUP BY 1", domain),
            ("SELECT NULL::d GROUP BY ()", domain),
            ("SELECT 1 INTERSECT SELECT NULL::d GROUP BY ()", domain),
            (
                "SELECT 1 INTERSECT SELECT NULL::d GROUP BY GROUPING SETS (())",
                domain,
            ),
            ("SELECT NULL::d GROUP BY GROUPING SETS ((), ())", domain),
            ("SELECT 1 INTERSECT SELECT DISTINCT NULL::d", domain),
            ("SELECT 1 EXCEPT SELECT NULL::d GROUP BY 1", domain),
            (
                "SELECT 1 INTERSECT SELECT NULL::d GROUP BY 1 HAVING false",
                None,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s ORDER BY a LIMIT 1",
                domain,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s ORDER BY 1 LIMIT 1",
                domain,
            ),
            (
                "SELECT a FROM (SELECT 1 AS a, 2 AS b UNION ALL SELECT NULL::d, 3) s \
                 ORDER BY b LIMIT 1",
                domain,
            ),
            (
                "SELECT x FROM (SELECT x FROM v UNION ALL SELECT 1) s ORDER BY 1 LIMIT 1",
                domain,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s LIMIT 1",
                None,
            ),
            // A key a WHERE equates to a constant is redundant
            // (pathkey_is_redundant): no Sort, the Limit stops in the
            // first arm. Any key whose columns a WHERE equality reads is
            // taken as such, as is a constant one.
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a = 1 ORDER BY a LIMIT 1",
                None,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a = 1 ORDER BY 1 LIMIT 1",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a = 1 ORDER BY a LIMIT 1",
                None,
            ),
            (
                "SELECT a AS z FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a = 1 ORDER BY z LIMIT 1",
                None,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a + 0 = 1 ORDER BY a + 0 LIMIT 1",
                None,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE NOT (a <> 1) ORDER BY a LIMIT 1",
                None,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a = 1 ORDER BY a, 1 LIMIT 1",
                None,
            ),
            (
                "SELECT a FROM (SELECT 1 AS a, 2 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE b = 2 ORDER BY b LIMIT 1",
                None,
            ),
            // An inequality makes no equivalence class: the Sort stays.
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a > 0 ORDER BY a LIMIT 1",
                domain,
            ),
            // The Sort reads every row of the subquery, which projects its
            // arms whole (make_sort_input_target postpones only the
            // query's own expensive expressions past the Limit).
            (
                "SELECT * FROM (SELECT 1 AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 ORDER BY b OFFSET 1",
                domain,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 ORDER BY b LIMIT 1 OFFSET 1",
                domain,
            ),
            // No Sort (the key is redundant): the Limit stops before the
            // second arm, or past it.
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a = 1 ORDER BY a LIMIT 2",
                domain,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a = 1 ORDER BY a LIMIT 1 OFFSET 1",
                domain,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s LIMIT 2",
                domain,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT 2 UNION ALL SELECT NULL::d) s \
                 LIMIT 2",
                None,
            ),
            // A row comparison is one equality per column; a duplicate OR
            // arm is dropped, leaving an equality.
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE (a, 1) = (1, 1) ORDER BY a LIMIT 1",
                None,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a = 1 OR a = 1 ORDER BY a LIMIT 1",
                None,
            ),
            // Only the expressions an equality compares are equated.
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a + 0 = 1 ORDER BY a LIMIT 1",
                domain,
            ),
            (
                "SELECT * FROM (SELECT 1 AS a UNION ALL SELECT NULL::d) s \
                 WHERE a = 1 ORDER BY a + 0 LIMIT 1",
                domain,
            ),
            // `b = 2` holds in both arms: every row reaches the Sort by a.
            (
                "SELECT a FROM (SELECT 1 AS a, 2 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE b = 2 ORDER BY a LIMIT 1",
                domain,
            ),
            (
                "SELECT a FROM (SELECT 1 AS a, 2 AS b UNION ALL SELECT NULL::d, 3) s \
                 WHERE b = 2 ORDER BY a LIMIT 1",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s WHERE a = 1",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s \
                 WHERE a IS NULL",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION SELECT 1, 2) s WHERE a = 1",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s \
                 WHERE s IS NULL",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION SELECT 1, 2) s WHERE s IS NULL",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b EXCEPT SELECT 1, 2) s WHERE b = 3",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b EXCEPT SELECT 1, 2) s \
                 WHERE b = 3 AND b > 2",
                domain,
            ),
            // Two equalities contradicting each other through their
            // equivalence class leave the scan dummy
            // (generate_base_implied_equalities_const).
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b EXCEPT SELECT 1, 2) s \
                 WHERE b = 3 AND b = 4",
                None,
            ),
            // Equal constants make no contradiction.
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b EXCEPT SELECT 1, 2) s \
                 WHERE b = 3 AND b = 3",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b, 2 AS c EXCEPT SELECT 1, 2, 3) s \
                 WHERE b = 3 AND c = 4",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b, 2 AS c EXCEPT SELECT 1, 2, 3) s \
                 WHERE b = c AND b = 3 AND c = 4",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2 LIMIT 5) s \
                 WHERE b = 3",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a EXCEPT SELECT 1) s",
                domain,
            ),
            (
                "SELECT count(*) FROM (SELECT NULL::d AS a UNION SELECT 1) s",
                domain,
            ),
            (
                "SELECT (SELECT 1 FROM (SELECT NULL::d AS a UNION SELECT 1) s LIMIT 1)",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s LIMIT 1",
                domain,
            ),
            // Pushed down into each arm, before its projection.
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s \
                 WHERE a = 1 AND b = 2",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT * FROM (SELECT NULL::d AS a, 1 AS b UNION ALL \
                 SELECT 1, 2) s1) s2 WHERE b IS NULL",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL (SELECT NULL::d, 2 \
                 UNION SELECT NULL::d, 4 ORDER BY 1)) s WHERE b = 7",
                None,
            ),
            // Folded to NULL (a custom plan's parameter is a constant): no
            // row is read.
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s \
                 WHERE a = NULL",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s \
                 WHERE a = $1",
                None,
            ),
            // Flattened, unused columns are dropped from each leaf.
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s",
                None,
            ),
            (
                "SELECT b FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s",
                None,
            ),
            (
                "SELECT b FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 ORDER BY b LIMIT 1",
                None,
            ),
            (
                "SELECT a FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s",
                domain,
            ),
            // A qualification pushed down into each leaf evaluates the
            // columns it reads there — not the whole row, whose NULL
            // test is one of each column (`b IS NULL` is false).
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE a = 1",
                domain,
            ),
            (
                "SELECT b FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE a IS NULL",
                domain,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE s IS NULL",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE b = 1",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE a = NULL",
                None,
            ),
            // `*` passes the set operation's columns through.
            (
                "SELECT 1 FROM (SELECT * FROM (SELECT NULL::d AS a, 1 AS b UNION ALL \
                 SELECT 1, 2) s1) s2",
                domain,
            ),
            (
                "SELECT b FROM (SELECT * FROM (SELECT NULL::d AS a, 1 AS b UNION ALL \
                 SELECT NULL::d, 2) s1) s2",
                None,
            ),
            (
                "SELECT b FROM (SELECT NULL::d AS a, 1 AS b UNION SELECT NULL::d, 2) s",
                domain,
            ),
        ],
    );
}

/// A WHERE over a set operation whose leaves are select lists over no FROM
/// item is pushed down into each leaf (to every one of a flattened UNION
/// ALL, through apply_child_basequals; to another set operation's where
/// `qual_is_pushdown_safe` lets it) with the leaf's expressions for its
/// columns, and folded there: a conjunct folded to false leaves the leaf
/// out, the others are its One-Time Filter, evaluated before its row is
/// projected. A leaf whose filter surely fails fails every execution
/// reading it (an Append runs them in order: past the rows of the leaves
/// before it, under a LIMIT). Over a set operation a WHERE isn't pushed
/// into, a duplicate OR arm, an IN list, equal constants (of one number
/// or literal), or equalities of different input types make no
/// contradiction that would leave the scan dummy.
#[test]
fn a_qualification_pushed_into_set_operation_leaves_fails_every_execution() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE DOMAIN d AS int NOT NULL;
         CREATE TABLE t (z int);
         CREATE VIEW v AS SELECT NULL::d AS x, 1 AS y;",
    )]);
    let domain = Some("domain d does not allow null values");
    let flat = "(SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s";
    let union = "(SELECT NULL::d AS a, 1 AS b UNION SELECT NULL::d, 2) s";
    let ordered = "(SELECT 1::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s";
    let except = "(SELECT NULL::d AS a, 1 AS b EXCEPT SELECT 1, 2) s";
    let mismatched = "(SELECT 1 AS a UNION ALL SELECT NULL::d) s";
    let cases: Vec<(String, Option<&str>)> = vec![
        // Each leaf of a flattened UNION ALL.
        (
            format!("SELECT 1 FROM {flat} WHERE a = 1 AND b = 2"),
            domain,
        ),
        (format!("SELECT 1 FROM {flat} WHERE b = 1 OR a = 1"), domain),
        (
            format!("SELECT 1 FROM {flat} WHERE coalesce(a, 5) = 1"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {flat} WHERE s.a IS NULL OR s.b = 0"),
            domain,
        ),
        (format!("SELECT 1 FROM {flat} WHERE s IS NOT NULL"), domain),
        (format!("SELECT 1 FROM {flat} WHERE NOT (a = 1)"), domain),
        (format!("SELECT 1 FROM {flat} WHERE a = 1 OR a = 2"), domain),
        (
            format!("SELECT 1 FROM {flat} WHERE a IS NOT DISTINCT FROM 1"),
            domain,
        ),
        // A volatile conjunct is a One-Time Filter below the others'.
        (
            format!("SELECT 1 FROM {flat} WHERE a = 1 AND random() > 2"),
            domain,
        ),
        (
            "SELECT 1 FROM (SELECT 1::d AS a, 1 AS b UNION ALL SELECT 1::d, 2 \
             UNION ALL SELECT NULL::d, 3) s WHERE b = 3 AND a = 1"
                .into(),
            domain,
        ),
        // No leaf, or every leaf left out.
        (format!("SELECT 1 FROM {flat} WHERE a = 1 AND b = 3"), None),
        (
            format!("SELECT 1 FROM {flat} WHERE a = 1 AND b = 1 AND b = 2"),
            None,
        ),
        (
            format!("SELECT 1 FROM {ordered} WHERE b = 3 AND a = 1"),
            None,
        ),
        // Only the columns read above are projected.
        (
            "SELECT b FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1::d, 2) s \
             WHERE b = 1"
                .into(),
            None,
        ),
        // Equalities of a column contradicting each other through their
        // equivalence class leave the scan dummy; restrictions of an
        // appendrel member contradicting each other leave it out
        // (relation_excluded_by_constraints), even of two columns with one
        // expression.
        (format!("SELECT 1 FROM {flat} WHERE a = 1 AND a = 2"), None),
        (format!("SELECT 1 FROM {flat} WHERE a = 1 AND a > 2"), None),
        (format!("SELECT 1 FROM {flat} WHERE a > 1 AND a < 0"), None),
        (format!("SELECT 1 FROM {flat} WHERE a = 1 AND a <> 1"), None),
        (
            format!("SELECT 1 FROM {flat} WHERE a = 1 AND NOT (a = 1)"),
            None,
        ),
        (
            format!("SELECT 1 FROM {flat} WHERE a IS NULL AND a IS NOT NULL"),
            None,
        ),
        (
            format!("SELECT 1 FROM {flat} WHERE a = 1 AND a IS NULL"),
            None,
        ),
        (
            format!("SELECT 1 FROM {flat} WHERE (a = 1 AND a = 2) OR b = 5"),
            None,
        ),
        (
            "SELECT 1 FROM (SELECT NULL::d AS a, NULL::d AS c, 1 AS b UNION ALL \
             SELECT NULL::d, NULL::d, 2) s WHERE a = 1 AND c = 2"
                .into(),
            None,
        ),
        (format!("SELECT 1 FROM {union} WHERE a = 1 AND a = 2"), None),
        // An integer and a numeric equality share no class.
        (
            format!("SELECT 1 FROM {union} WHERE a = 1 AND a = 1.5"),
            domain,
        ),
        (
            "SELECT 1 FROM (SELECT NULL::d AS a LIMIT 1) s WHERE a = 1 AND a = 2".into(),
            None,
        ),
        (
            format!("SELECT 1 FROM {flat} WHERE a = 1 AND a = 1"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {flat} WHERE a = 1 AND b = 1"),
            domain,
        ),
        // Another set operation's leaves are no appendrel members.
        (
            format!("SELECT 1 FROM {union} WHERE a = 1 AND a > 2"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {union} WHERE a > 1 AND a < 0"),
            domain,
        ),
        // Pulled up, a select list over no FROM item leaves no Var: each
        // conjunct a gating qual.
        ("SELECT 1 FROM v WHERE x = 1 AND x = 2".into(), domain),
        (
            "SELECT 1 FROM v WHERE x IS NULL AND x IS NOT NULL".into(),
            domain,
        ),
        (
            "SELECT 1 FROM (SELECT NULL::d AS a, NULL::d AS c) s WHERE a = 1 AND c = 2".into(),
            domain,
        ),
        // The other table may be empty, or the LIMIT reached before.
        (format!("SELECT 1 FROM {flat}, t WHERE a = 1"), None),
        (
            format!("SELECT * FROM {mismatched}, t WHERE a = 1 ORDER BY a LIMIT 2"),
            None,
        ),
        // Pushed down into another set operation's leaves, each projects
        // every column.
        (format!("SELECT 1 FROM {union} WHERE a = 1"), domain),
        (
            format!("SELECT 1 FROM {union} WHERE b = 1 OR a = 1"),
            domain,
        ),
        (
            "SELECT b FROM (SELECT NULL::d AS a, 1 AS b UNION SELECT 1, 2) s WHERE b = 1".into(),
            domain,
        ),
        (
            "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b INTERSECT SELECT NULL::d, 2) s \
             WHERE a = 1"
                .into(),
            domain,
        ),
        (
            "SELECT a FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT 1, 2) s WHERE b = 1"
                .into(),
            domain,
        ),
        (
            "SELECT b FROM (SELECT 1 AS b, NULL::d AS a UNION ALL SELECT 2, 1) s \
             WHERE b = 1 LIMIT 1"
                .into(),
            domain,
        ),
        (
            "SELECT b FROM (SELECT 1 AS b, NULL::d AS a UNION ALL SELECT 2, 1) s \
             WHERE b = 2 LIMIT 1"
                .into(),
            None,
        ),
        (
            "SELECT b FROM (SELECT 1 AS b, NULL::d AS a UNION SELECT 2, 1) s WHERE b = 3".into(),
            None,
        ),
        // A volatile conjunct may run first in the leaf's filter.
        (
            format!("SELECT 1 FROM {union} WHERE a = 1 AND random() > 2"),
            None,
        ),
        // An Append runs the leaves in order; a Sort reads them all.
        (
            format!("SELECT 1 FROM {ordered} WHERE b = 2 AND a = 1 LIMIT 1"),
            domain,
        ),
        (
            format!("SELECT * FROM {ordered} WHERE a = 1 ORDER BY b LIMIT 1"),
            domain,
        ),
        (
            format!("SELECT * FROM {ordered} WHERE a = 1 ORDER BY a LIMIT 2"),
            domain,
        ),
        (
            format!("SELECT a FROM {ordered} WHERE b IN (1, 2) LIMIT 2"),
            domain,
        ),
        (format!("SELECT 1 FROM {ordered} WHERE a = 1 LIMIT 1"), None),
        (
            format!("SELECT * FROM {ordered} WHERE a = 1 ORDER BY a LIMIT 1"),
            None,
        ),
        (
            format!("SELECT 1 FROM {ordered} WHERE b = 2 OR a = 1 LIMIT 1"),
            None,
        ),
        (format!("SELECT 1 FROM {ordered} WHERE b = 1 LIMIT 2"), None),
        (
            format!("SELECT a FROM {ordered} WHERE b IN (1, 2) LIMIT 1"),
            None,
        ),
        (
            "SELECT 1 FROM (SELECT 1::d AS a, 1 AS b UNION ALL SELECT 1::d, 2 \
             UNION ALL SELECT NULL::d, 3) s WHERE b > 1 AND a = 1 LIMIT 1"
                .into(),
            None,
        ),
        // Over projected rows, an IN list or a duplicate OR arm is an
        // equality (the Sort by `a` dropped), past the first arm's row.
        (
            format!("SELECT * FROM {mismatched} WHERE a IN (1) ORDER BY a LIMIT 2"),
            domain,
        ),
        (
            format!("SELECT * FROM {mismatched} WHERE a = 1 OR a = 1 ORDER BY a LIMIT 2"),
            domain,
        ),
        // Equal constants contradict nothing; an OR enters no equivalence
        // class but where its arms repeat one.
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 AND b = 3.0"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 AND b = 3::int8"),
            domain,
        ),
        // An integer equality and a numeric one put `b` and `b::numeric`
        // in different equivalence classes: different constants
        // contradict nothing.
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 AND b = 3.5"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b = 3.5 AND b = 3"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b IN (3) AND 3.5 = b"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 AND b = '3.5'::numeric"),
            domain,
        ),
        (
            "SELECT 1 FROM (SELECT NULL::d AS a, 1::int2 AS b EXCEPT SELECT 1, 2::int2) s \
             WHERE b = 3 AND b = 3.5"
                .into(),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 AND b = ' 3 '"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b IN (3) AND b = 3"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 OR b = 3"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 OR b = 4"),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE a = 1 OR b = 2"),
            domain,
        ),
        (
            "SELECT 1 FROM (SELECT NULL::d AS a, 1::numeric AS b EXCEPT SELECT 1, 2) s \
             WHERE b = 3 AND b = 3.000"
                .into(),
            domain,
        ),
        (
            "SELECT 1 FROM (SELECT NULL::d AS a, 'x'::text AS b EXCEPT SELECT 1, 'y') s \
             WHERE b = 'x' AND b = 'x'"
                .into(),
            domain,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE (b = 3 OR b = 3) AND b = 4"),
            None,
        ),
        // Equalities of one input type share a class, whatever the
        // constant's type (`int4 = int8` is of the integer family).
        (
            format!("SELECT 1 FROM {except} WHERE b = 3.5 AND b = 4.5"),
            None,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 AND b = 4::int8"),
            None,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 AND b = 3.5 AND b = 4"),
            None,
        ),
        (
            format!("SELECT 1 FROM {except} WHERE b = 3 AND b = 3.0 AND b = 4"),
            None,
        ),
        (
            "SELECT 1 FROM (SELECT NULL::d AS a, 1::int2 AS b EXCEPT SELECT 1, 2::int2) s \
             WHERE b = 3 AND b = 4::int8"
                .into(),
            None,
        ),
        (
            "SELECT 1 FROM (SELECT NULL::d AS a, 'x'::text AS b EXCEPT SELECT 1, 'y') s \
             WHERE b = '03' AND b = '3'"
                .into(),
            None,
        ),
    ];
    let cases: Vec<(&str, Option<&str>)> = cases.iter().map(|(q, e)| (q.as_str(), *e)).collect();
    assert_outcomes(&db, &cases);
}

/// Set operations PostgreSQL 18 reads whole on every execution, which the
/// analyzer doesn't prove and accepts (it never rejects what it can't
/// prove fails):
/// - a conjunct whose value only the execution knows (`random() < 2`,
///   `now() > '2000-01-01'`), which shares the leaf's One-Time Filter;
/// - an equality of two columns, whose class the planner fills with
///   derived equalities;
/// - two failing restrictions of a flattened UNION ALL's leaf that don't
///   contradict each other (`a = 1 AND a = 1.0`, or `a = 1 AND a = 1.5`,
///   of different operand expressions): the analyzer doesn't tell which
///   ones `predicate_refuted_by` proves contradictory.
#[test]
fn known_gaps_in_set_operations_read_whole() {
    let db = build_db(&[("0001.sql", "CREATE DOMAIN d AS int NOT NULL;")]);
    assert_outcomes(
        &db,
        &[
            (
                "SELECT b FROM (SELECT 1 AS b, NULL::d AS a UNION SELECT 2, 1) s \
                 WHERE b = 1 AND random() < 2",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE a = 1 AND now() > '2000-01-01'",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE a = b AND a = 1",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE a = 1 AND a = 1.0",
                None,
            ),
            (
                "SELECT 1 FROM (SELECT NULL::d AS a, 1 AS b UNION ALL SELECT NULL::d, 2) s \
                 WHERE a = 1 AND a = 1.5",
                None,
            ),
        ],
    );
}
