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
