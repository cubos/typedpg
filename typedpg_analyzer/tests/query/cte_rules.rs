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
