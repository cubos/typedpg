//! CTE visibility (sublinks, data-modifying CTE chains) and PG's WITH
//! validation rules.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, b text, c varchar(10) NOT NULL, d numeric(10,2));
         CREATE TABLE u (id int PRIMARY KEY, t_id int NOT NULL, x text NOT NULL, y int);",
    )
    .unwrap();
    db
}

// ── CTEs are visible inside sublinks ─────────────────────────────────────────

#[test]
fn cte_visible_in_in_sublink() {
    let db = setup();
    let s = db
        .analyze("WITH w AS (SELECT a FROM t) SELECT * FROM t WHERE a IN (SELECT a FROM w)")
        .unwrap();
    assert_eq!(s.columns.len(), 5);
    db.analyze("WITH w AS (SELECT a FROM t) SELECT * FROM t WHERE a IN (TABLE w)")
        .unwrap();
}

#[test]
fn cte_visible_in_scalar_and_exists_sublinks() {
    let db = setup();
    let s = db
        .analyze("WITH w AS (SELECT a FROM t) SELECT (SELECT max(a) FROM w)")
        .unwrap();
    assert_cols(&s, vec![cn("max", int4())]);
    db.analyze(
        "WITH w AS (SELECT a FROM t) SELECT id FROM t WHERE EXISTS (SELECT 1 FROM w WHERE w.a = t.a)",
    )
    .unwrap();
    // Through a FROM subquery nested in the sublink, and in VALUES.
    db.analyze(
        "WITH w AS (SELECT a FROM t) SELECT id FROM t WHERE a IN (SELECT s.a FROM (SELECT a FROM w) s)",
    )
    .unwrap();
    let s = db
        .analyze("WITH w AS (SELECT a FROM t) VALUES ((SELECT max(a) FROM w))")
        .unwrap();
    assert_cols(&s, vec![cn("column1", int4())]);
}

#[test]
fn cte_visible_in_dml_sublinks() {
    let db = setup();
    db.analyze("WITH w AS (SELECT id FROM t) DELETE FROM u WHERE t_id IN (SELECT id FROM w)")
        .unwrap();
    db.analyze(
        "WITH w AS (SELECT id FROM t) UPDATE u SET y = 1 WHERE t_id IN (SELECT id FROM w) RETURNING id",
    )
    .unwrap();
    db.analyze(
        "WITH w AS (SELECT id FROM t) INSERT INTO u (id, t_id, x) VALUES (1, (SELECT max(id) FROM w), 'x')",
    )
    .unwrap();
    db.analyze(
        "WITH d AS (DELETE FROM u RETURNING t_id) DELETE FROM t WHERE id IN (SELECT t_id FROM d)",
    )
    .unwrap();
}

/// A data-modifying CTE body sees the CTEs defined before it.
#[test]
fn data_modifying_cte_chain() {
    let db = setup();
    let s = db
        .analyze(
            "WITH ins AS (INSERT INTO t (id, a, c) VALUES (1, 2, 'x') RETURNING id), \
                  upd AS (UPDATE u SET y = 1 WHERE t_id IN (SELECT id FROM ins) RETURNING y) \
             SELECT * FROM upd",
        )
        .unwrap();
    assert_cols(&s, vec![cn("y", int4())]);
    let s = db
        .analyze(
            "WITH ins AS (INSERT INTO t (id, a, c) VALUES (1, 2, 'x') RETURNING id), \
                  del AS (DELETE FROM u USING ins WHERE u.t_id = ins.id RETURNING u.id) \
             SELECT * FROM del",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int4())]);
}

// ── SEARCH / CYCLE ───────────────────────────────────────────────────────────

fn setup_tree() -> PgCatalog {
    let mut db = setup();
    db.apply_sql("CREATE TABLE tree (id int PRIMARY KEY, parent int);")
        .unwrap();
    db
}

const TREE_CTE: &str = "WITH RECURSIVE r AS (SELECT id, parent FROM tree WHERE parent IS NULL \
     UNION ALL SELECT tree.id, tree.parent FROM tree JOIN r ON tree.parent = r.id)";

/// PG 18: the DEPTH FIRST sequence column is `record[]` (the path of
/// visited rows), BREADTH FIRST's is `record`.
#[test]
fn search_sequence_column_types() {
    let db = setup_tree();
    let s = db
        .analyze(&format!(
            "{TREE_CTE} SEARCH DEPTH FIRST BY id SET ord SELECT * FROM r"
        ))
        .unwrap();
    assert_eq!(col(&s, "ord").pg_type, basic("pg_catalog", "_record"));
    assert!(!col(&s, "ord").nullable);
    let s = db
        .analyze(&format!(
            "{TREE_CTE} SEARCH BREADTH FIRST BY id SET ord SELECT * FROM r"
        ))
        .unwrap();
    assert_eq!(col(&s, "ord").pg_type, basic("pg_catalog", "record"));
}

/// The cycle mark takes the common type of its TO / DEFAULT values.
#[test]
fn cycle_mark_column_type_follows_its_values() {
    let db = setup_tree();
    let s = db
        .analyze(&format!(
            "{TREE_CTE} CYCLE id SET is_cycle TO 'Y' DEFAULT 'N' USING path SELECT * FROM r"
        ))
        .unwrap();
    assert_col!(&s, "is_cycle", text(), nullable = false);
    let s = db
        .analyze(
            "WITH RECURSIVE r(n) AS (SELECT a FROM t UNION ALL SELECT n FROM r) \
             CYCLE n SET cyc TO 1 DEFAULT 0 USING p SELECT cyc FROM r",
        )
        .unwrap();
    assert_cols(&s, vec![c("cyc", int4())]);
    let s = db
        .analyze(
            "WITH RECURSIVE r(n) AS (SELECT a FROM t UNION ALL SELECT n FROM r) \
             CYCLE n SET cyc USING p SELECT cyc FROM r",
        )
        .unwrap();
    assert_cols(&s, vec![c("cyc", bool_ty())]);
}

#[test]
fn cycle_mark_value_errors() {
    let db = setup_tree();
    let err = db
        .analyze(
            "WITH RECURSIVE r(n) AS (SELECT a FROM t UNION ALL SELECT n FROM r) \
             CYCLE n SET cyc TO 1 DEFAULT 'x' USING p SELECT cyc FROM r",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("invalid input syntax for type integer: \"x\""),
        "{err}"
    );
    let err = db
        .analyze(
            "WITH RECURSIVE r(n) AS (SELECT a FROM t UNION ALL SELECT n FROM r) \
             CYCLE n SET cyc TO true DEFAULT 0 USING p SELECT * FROM r",
        )
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::DatatypeMismatch(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("CYCLE types boolean and integer cannot be matched"),
        "{err}"
    );
}

#[test]
fn search_cycle_validation() {
    let db = setup_tree();
    let cases: Vec<(String, &str)> = vec![
        (
            format!("{TREE_CTE} SEARCH DEPTH FIRST BY nosuch SET ord SELECT * FROM r"),
            "search column \"nosuch\" not in WITH query column list",
        ),
        (
            "WITH w AS (SELECT 1) SEARCH DEPTH FIRST BY n SET o SELECT 1".into(),
            "WITH query is not recursive",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT a FROM t) SEARCH DEPTH FIRST BY n SET o SELECT 1"
                .into(),
            "WITH query is not recursive",
        ),
        (
            "WITH w AS (SELECT 1 x) CYCLE x SET c USING p SELECT 1".into(),
            "WITH query is not recursive",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT a FROM t UNION ALL SELECT n FROM r) \
             CYCLE nosuch SET cyc USING p SELECT * FROM r"
                .into(),
            "cycle column \"nosuch\" not in WITH query column list",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT a FROM t UNION ALL SELECT n FROM r) \
             CYCLE n SET cyc USING cyc SELECT * FROM r"
                .into(),
            "cycle mark column name and cycle path column name are the same",
        ),
    ];
    for (sql, msg) in cases {
        let err = db.analyze(&sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::SyntaxError(_)),
            "{sql}: {err:?}"
        );
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
}

// ── Outer references from CTE bodies ─────────────────────────────────────────

/// A CTE body is a sub-level of the query owning the WITH: it sees the
/// enclosing levels — a sublink's outer query, a LATERAL subquery's
/// left-hand FROM items — as outer references.
#[test]
fn nested_cte_body_sees_enclosing_levels() {
    let db = setup();
    let s = db
        .analyze("SELECT (WITH z AS (SELECT t.a) SELECT a FROM z) FROM t")
        .unwrap();
    assert_eq!(s.columns.len(), 1);
    let s = db
        .analyze("SELECT * FROM t, LATERAL (WITH z AS (SELECT t.a) SELECT * FROM z) q")
        .unwrap();
    assert_eq!(s.columns.len(), 6);
    db.analyze("SELECT * FROM t WHERE EXISTS (WITH z AS (SELECT t.a) SELECT * FROM z)")
        .unwrap();
    db.analyze(
        "SELECT (WITH RECURSIVE r(n) AS (SELECT t.a UNION ALL SELECT n + 1 FROM r WHERE n < 3) \
         SELECT max(n) FROM r) FROM t",
    )
    .unwrap();
    // The query's own FROM items are not visible to its CTEs.
    let err = db
        .analyze("WITH z AS (SELECT t.a) SELECT * FROM z, t")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("missing FROM-clause entry for table \"t\""),
        "{err}"
    );
}

// ── WITH validation ──────────────────────────────────────────────────────────

#[test]
fn with_clause_validation() {
    let db = setup();
    let cases: &[(&str, &str)] = &[
        (
            "WITH ins AS (INSERT INTO t (id, a, c) VALUES (1, 2, 'x')) SELECT * FROM ins",
            "WITH query \"ins\" does not have a RETURNING clause",
        ),
        (
            "SELECT * FROM (WITH ins AS (INSERT INTO t (id, a, c) VALUES (1, 2, 'x') RETURNING id) \
             SELECT * FROM ins) s",
            "WITH clause containing a data-modifying statement must be at the top level",
        ),
        (
            "SELECT (WITH d AS (DELETE FROM t RETURNING id) SELECT count(*) FROM d)",
            "WITH clause containing a data-modifying statement must be at the top level",
        ),
        (
            "WITH x AS (WITH d AS (DELETE FROM t RETURNING id) SELECT * FROM d) SELECT * FROM x",
            "WITH clause containing a data-modifying statement must be at the top level",
        ),
        (
            "WITH w AS (SELECT a FROM t), w AS (SELECT 1) SELECT * FROM w",
            "WITH query name \"w\" specified more than once",
        ),
        (
            "WITH w(z, zz) AS (SELECT a FROM t) SELECT z FROM w",
            "WITH query \"w\" has 1 columns available but 2 columns specified",
        ),
    ];
    for (sql, msg) in cases {
        let err = db.analyze(sql).unwrap_err();
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
    // An unreferenced data-modifying CTE needs no RETURNING.
    db.analyze("WITH ins AS (INSERT INTO t (id, a, c) VALUES (1, 2, 'x')) SELECT 1")
        .unwrap();
}

// ── Recursive-query structure ────────────────────────────────────────────────

#[test]
fn recursive_query_structure_rules() {
    let db = setup();
    let cases: &[(&str, &str)] = &[
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT count(*)::int FROM r) SELECT n FROM r",
            "aggregate functions are not allowed in a recursive query's recursive term",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT r.n FROM r, r r2) SELECT n FROM r",
            "recursive reference to query \"r\" must not appear more than once",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r ORDER BY 1) SELECT n FROM r",
            "ORDER BY in a recursive query is not implemented",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r LIMIT 3) SELECT n FROM r",
            "LIMIT in a recursive query is not implemented",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 INTERSECT SELECT n + 1 FROM r) SELECT n FROM r",
            "recursive query \"r\" does not have the form non-recursive-term UNION [ALL] recursive-term",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t LEFT JOIN r ON true) SELECT n FROM r",
            "recursive reference to query \"r\" must not appear within an outer join",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT n FROM r UNION ALL SELECT 1) SELECT n FROM r",
            "recursive reference to query \"r\" must not appear within its non-recursive term",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE a IN (SELECT n FROM r)) SELECT n FROM r",
            "recursive reference to query \"r\" must not appear within a subquery",
        ),
    ];
    for (sql, msg) in cases {
        let err = db.analyze(sql).unwrap_err();
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
    for sql in [
        // Not self-referencing: an ordinary INTERSECT.
        "WITH RECURSIVE r(n) AS (SELECT 1 INTERSECT SELECT 2) SELECT n FROM r",
        // The CTE on the preserved side of an outer join, or in a FROM
        // subquery, is fine.
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r LEFT JOIN t ON true WHERE n < 3) SELECT n FROM r",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM (SELECT * FROM r) s WHERE n < 3) SELECT n FROM r",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// transformWithClause: in WITH RECURSIVE every item sees every other, so
/// the items are analyzed in dependency order (TopologicalSort); a cycle
/// between items is 0A000.
#[test]
fn recursive_with_items_see_later_siblings() {
    let db = setup();
    let s = db
        .analyze("WITH RECURSIVE x AS (SELECT * FROM y), y AS (SELECT 1 a) SELECT * FROM x")
        .unwrap();
    assert_cols(&s, vec![c("a", int4())]);
    db.analyze(
        "WITH RECURSIVE x AS (SELECT * FROM y), y(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM y \
         WHERE n < 3), z AS (SELECT * FROM x) SELECT * FROM z",
    )
    .unwrap();
    // An inner WITH redefining the name captures the reference.
    db.analyze(
        "WITH RECURSIVE x AS (WITH y AS (SELECT 2 a) SELECT * FROM y), y AS (SELECT * FROM x) \
         SELECT * FROM y",
    )
    .unwrap();
    let err = db
        .analyze(
            "WITH RECURSIVE x(a) AS (SELECT * FROM y), y(a) AS (SELECT 1 UNION ALL SELECT a FROM x) \
             SELECT * FROM x",
        )
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::FeatureNotSupported(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("mutual recursion between WITH items is not implemented"),
        "{err}"
    );
    // Without RECURSIVE a later item stays invisible.
    let err = db
        .analyze("WITH x AS (SELECT * FROM y), y AS (SELECT 1 a) SELECT * FROM x")
        .unwrap_err();
    assert!(
        err.to_string().starts_with("relation \"y\" does not exist"),
        "{err}"
    );
}

/// checkWellFormedSelectStmt: only INTERSECT ALL is unsafe for the
/// recursive reference; EXCEPT is unsafe on its right side, and on its left
/// side with ALL.
#[test]
fn recursive_reference_under_intersect_and_except() {
    let db = setup();
    for sql in [
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n FROM r INTERSECT SELECT 2)) \
         SELECT * FROM r",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT 2 INTERSECT SELECT n FROM r)) \
         SELECT * FROM r",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n FROM r EXCEPT SELECT 2)) \
         SELECT * FROM r",
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(s.columns.len(), 1, "{sql}");
    }
    for (sql, msg) in [
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n FROM r INTERSECT ALL SELECT 2)) \
             SELECT * FROM r",
            "recursive reference to query \"r\" must not appear within INTERSECT",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n FROM r EXCEPT ALL SELECT 2)) \
             SELECT * FROM r",
            "recursive reference to query \"r\" must not appear within EXCEPT",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT 2 EXCEPT SELECT n FROM r)) \
             SELECT * FROM r",
            "recursive reference to query \"r\" must not appear within EXCEPT",
        ),
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(matches!(err, AnalyzeError::InvalidRecursion(_)), "{sql}: {err:?}");
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
}

/// checkWellFormedRecursion walks a WITH attached to the recursive query's
/// UNION as a subquery context; a WITH there that doesn't reference the
/// CTE is visible to both terms.
#[test]
fn with_on_a_recursive_query_body() {
    let db = setup();
    let err = db
        .analyze(
            "WITH RECURSIVE r(n) AS (WITH x AS (SELECT * FROM r) SELECT 1 UNION ALL \
             SELECT n FROM x) SELECT * FROM r",
        )
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::InvalidRecursion(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("recursive reference to query \"r\" must not appear within a subquery"),
        "{err}"
    );
    let s = db
        .analyze(
            "WITH RECURSIVE r(n) AS (WITH x AS (SELECT 1 AS k) SELECT k FROM x UNION ALL \
             SELECT n + k FROM r, x WHERE n < 3) SELECT * FROM r",
        )
        .unwrap();
    assert_cols(&s, vec![c("n", int4())]);
}

/// analyzeCTETargetList exposes a recursive CTE's `unknown` column (an
/// untyped parameter or literal in the non-recursive term) as text before
/// the recursive term is analyzed. (With `WHERE n < 5` the query has a
/// second error, which PG and the analyzer may report in either order.)
#[test]
fn unknown_non_recursive_term_column_is_text() {
    let db = setup();
    let err = db
        .analyze(
            "WITH RECURSIVE r(n) AS (SELECT $a UNION ALL SELECT n + 1 FROM r WHERE length(n) < 5) \
             SELECT * FROM r",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("operator does not exist: text + integer"),
        "{err}"
    );
    let s = db
        .analyze(
            "WITH RECURSIVE r(n) AS (SELECT $a UNION ALL SELECT n || 'x' FROM r WHERE length(n) < 5) \
             SELECT * FROM r",
        )
        .unwrap();
    assert_eq!(s.columns[0].pg_type, text());
    assert_eq!(s.params[0].pg_type, text());
}

/// analyzeCTETargetList applies a CTE's column alias list to a
/// data-modifying body's RETURNING columns too.
#[test]
fn data_modifying_cte_column_aliases() {
    let db = setup();
    let s = db
        .analyze("WITH d(x) AS (DELETE FROM t RETURNING id, a) SELECT x, a FROM d")
        .unwrap();
    assert_cols(&s, vec![c("x", int4()), c("a", int4())]);
    let err = db
        .analyze("WITH d(x, y, z) AS (DELETE FROM t RETURNING id, a) SELECT 1 FROM d")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("WITH query \"d\" has 2 columns available but 3 columns specified"),
        "{err}"
    );
}

/// analyzeCTE's SEARCH / CYCLE name checks, and the columns those clauses
/// add to every reference — including the recursive term's self-reference,
/// where a clashing name makes the column reference ambiguous.
#[test]
fn search_cycle_column_names() {
    let db = setup();
    const R: &str = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5)";
    let cases: &[(&str, &str)] = &[
        (
            "SEARCH DEPTH FIRST BY n, n SET o",
            "search column \"n\" specified more than once",
        ),
        (
            "CYCLE n, n SET c USING p",
            "cycle column \"n\" specified more than once",
        ),
        (
            "SEARCH DEPTH FIRST BY n SET o CYCLE n SET o USING p",
            "search sequence column name and cycle mark column name are the same",
        ),
        (
            "SEARCH DEPTH FIRST BY n SET o CYCLE n SET c USING o",
            "search sequence column name and cycle path column name are the same",
        ),
        (
            "SEARCH DEPTH FIRST BY n SET n",
            "column reference \"n\" is ambiguous",
        ),
        ("CYCLE n SET n USING p", "column reference \"n\" is ambiguous"),
        ("CYCLE n SET c USING n", "column reference \"n\" is ambiguous"),
    ];
    for (clause, msg) in cases {
        let sql = format!("{R} {clause} SELECT * FROM r");
        let err = db.analyze(&sql).unwrap_err();
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
    // Without a reference to clash with, the name check itself fires.
    let err = db
        .analyze(
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT 2 FROM r WHERE false) \
             SEARCH DEPTH FIRST BY n SET n SELECT * FROM r",
        )
        .unwrap_err();
    assert!(
        err.to_string().starts_with(
            "search sequence column name \"n\" already used in WITH query column list"
        ),
        "{err}"
    );
    // The recursive term sees the added columns but `*` there leaves them
    // out; so does every reference below the WITH's own level.
    db.analyze(&format!(
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE o IS NOT NULL \
         AND n < 5) SEARCH DEPTH FIRST BY n SET o SELECT * FROM r"
    ))
    .unwrap();
    for (sql, width) in [
        (format!("{R} SEARCH DEPTH FIRST BY n SET o SELECT * FROM r"), 2),
        (
            format!("{R} SEARCH DEPTH FIRST BY n SET o, s AS (SELECT * FROM r) SELECT * FROM s"),
            1,
        ),
        (
            format!("{R} SEARCH DEPTH FIRST BY n SET o SELECT * FROM r UNION ALL SELECT * FROM r"),
            1,
        ),
        (
            format!("{R} CYCLE n SET c USING p SELECT * FROM (SELECT * FROM r) q"),
            1,
        ),
    ] {
        let s = db.analyze(&sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(s.columns.len(), width, "{sql}");
    }
}
