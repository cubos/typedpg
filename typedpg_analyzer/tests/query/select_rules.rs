//! SELECT-level rules PG enforces during parse analysis: locking clauses,
//! SELECT INTO, DISTINCT ON / ORDER BY USING / LIMIT placement, DEFAULT
//! outside INSERT, set-operation ORDER BY / LIMIT, and implicit column names.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, b text);
         CREATE TABLE u (id int PRIMARY KEY, t_id int NOT NULL, x text NOT NULL);",
    )
    .unwrap();
    db
}

#[track_caller]
fn assert_err_prefix(db: &PgCatalog, sql: &str, msg: &str) -> AnalyzeError {
    let err = db
        .analyze(sql)
        .expect_err(&format!("expected an error for: {sql}"));
    assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    err
}

// ── FOR UPDATE / SHARE ───────────────────────────────────────────────────────

/// PG's `CheckSelectLocking` (0A000).
#[test]
fn locking_clause_restrictions() {
    let db = setup();
    let cases = [
        (
            "SELECT DISTINCT a FROM t FOR UPDATE",
            "FOR UPDATE is not allowed with DISTINCT clause",
        ),
        (
            "SELECT count(*) FROM t FOR UPDATE",
            "FOR UPDATE is not allowed with aggregate functions",
        ),
        (
            "SELECT a FROM t GROUP BY a FOR UPDATE",
            "FOR UPDATE is not allowed with GROUP BY clause",
        ),
        (
            "SELECT a FROM t GROUP BY a HAVING count(*) > 1 FOR SHARE",
            "FOR SHARE is not allowed with GROUP BY clause",
        ),
        (
            "SELECT a FROM t UNION SELECT a FROM t FOR UPDATE",
            "FOR UPDATE is not allowed with UNION/INTERSECT/EXCEPT",
        ),
        (
            "SELECT a, row_number() OVER () FROM t FOR UPDATE",
            "FOR UPDATE is not allowed with window functions",
        ),
        (
            "SELECT a FROM t ORDER BY count(*) OVER () FOR UPDATE",
            "FOR UPDATE is not allowed with window functions",
        ),
        (
            "SELECT generate_series(1,2) FOR UPDATE",
            "FOR UPDATE is not allowed with set-returning functions in the target list",
        ),
        (
            "SELECT a FROM (SELECT DISTINCT a FROM t) s FOR UPDATE",
            "FOR UPDATE is not allowed with DISTINCT clause",
        ),
        (
            "SELECT * FROM (SELECT count(*) FROM t) s FOR KEY SHARE OF s",
            "FOR KEY SHARE is not allowed with aggregate functions",
        ),
    ];
    for (sql, msg) in cases {
        let err = assert_err_prefix(&db, sql, msg);
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
    }
}

/// `transformLockingClause`: a named entry must be lockable.
#[test]
fn locking_clause_targets() {
    let db = setup();
    let cases = [
        (
            "WITH w AS (SELECT * FROM t) SELECT * FROM w FOR UPDATE OF w",
            "FOR UPDATE cannot be applied to a WITH query",
        ),
        (
            "SELECT * FROM t, generate_series(1,2) g FOR UPDATE OF g",
            "FOR UPDATE cannot be applied to a function",
        ),
        (
            "SELECT * FROM t JOIN u USING (id) AS j FOR UPDATE OF j",
            "FOR UPDATE cannot be applied to a join",
        ),
    ];
    for (sql, msg) in cases {
        let err = assert_err_prefix(&db, sql, msg);
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
    }
    // Accepted: unnamed CTE / function entries are skipped, aggregates in
    // a sublink don't count, other entries may be named.
    for sql in [
        "WITH w AS (SELECT * FROM t) SELECT * FROM w FOR UPDATE",
        "SELECT * FROM (VALUES (1)) v FOR UPDATE OF v",
        "SELECT * FROM (SELECT * FROM t) s FOR UPDATE OF s",
        "SELECT * FROM (SELECT DISTINCT a FROM t) s, t t2 FOR UPDATE OF t2",
        "SELECT (SELECT count(*) FROM u) FROM t FOR UPDATE",
        "SELECT * FROM t LEFT JOIN u ON u.t_id = t.id FOR UPDATE OF t",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // The planner rejects locking the nullable side of an outer join (PG
    // raises it when the prepared statement is executed).
    for sql in [
        "SELECT * FROM t LEFT JOIN u ON u.t_id = t.id FOR UPDATE",
        "SELECT * FROM t LEFT JOIN u ON u.t_id = t.id FOR UPDATE OF u",
    ] {
        let err = assert_err_prefix(
            &db,
            sql,
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
        );
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
    }
}
