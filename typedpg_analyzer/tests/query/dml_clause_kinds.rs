//! Aggregate, window, grouping and set-returning calls in the clauses of
//! INSERT / UPDATE / DELETE / MERGE — PG checks them per `ParseExprKind`
//! (`check_agglevels_and_constraints`, `transformWindowFuncCall`,
//! `check_srf_call_placement`). Expectations observed on PostgreSQL 18.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL, w text);")
        .unwrap();
    db
}

#[test]
fn placement_errors() {
    let db = setup();
    for (sql, msg) in [
        (
            "UPDATE t SET v = count(*)",
            "aggregate functions are not allowed in UPDATE",
        ),
        (
            "UPDATE t SET v = sum(v) OVER ()",
            "window functions are not allowed in UPDATE",
        ),
        (
            "UPDATE t SET v = 1 WHERE count(*) > 1",
            "aggregate functions are not allowed in WHERE",
        ),
        (
            "DELETE FROM t WHERE sum(1) OVER () > 1",
            "window functions are not allowed in WHERE",
        ),
        (
            "INSERT INTO t VALUES (1, count(*), 'x')",
            "aggregate functions are not allowed in VALUES",
        ),
        (
            "INSERT INTO t VALUES (7, sum(1) OVER (), 'x')",
            "window functions are not allowed in VALUES",
        ),
        (
            "INSERT INTO t VALUES (7, 1, 'x'), (8, count(*), 'y')",
            "aggregate functions are not allowed in VALUES",
        ),
        (
            "INSERT INTO t VALUES (1, 1, 'x') ON CONFLICT (id) DO UPDATE SET v = count(*)",
            "aggregate functions are not allowed in UPDATE",
        ),
        (
            "INSERT INTO t VALUES (7, 1, 'x') ON CONFLICT (id) DO UPDATE SET v = GROUPING(t.v)",
            "grouping operations are not allowed in UPDATE",
        ),
        (
            "INSERT INTO t VALUES (1, 1, 'x') ON CONFLICT (id) DO UPDATE SET v = generate_series(1, 2)",
            "set-returning functions are not allowed in UPDATE",
        ),
        (
            "INSERT INTO t VALUES (1, 1, 'x') ON CONFLICT (id) DO UPDATE SET v = 1 \
             WHERE count(*) > 1",
            "aggregate functions are not allowed in WHERE",
        ),
        (
            "INSERT INTO t VALUES (1, 1, 'x') ON CONFLICT (id) DO UPDATE SET v = 1 \
             WHERE generate_series(1, 2) > 1",
            "set-returning functions are not allowed in WHERE",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON t.id = s.x \
             WHEN MATCHED AND count(*) > 1 THEN DELETE",
            "aggregate functions are not allowed in MERGE WHEN conditions",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON t.id = s.x \
             WHEN MATCHED AND sum(1) OVER () > 1 THEN DELETE",
            "window functions are not allowed in MERGE WHEN conditions",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON t.id = s.x \
             WHEN MATCHED AND generate_series(1, 2) > 1 THEN DELETE",
            "set-returning functions are not allowed in MERGE WHEN conditions",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON t.id = s.x \
             WHEN MATCHED THEN UPDATE SET v = count(*)",
            "aggregate functions are not allowed in UPDATE",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON t.id = s.x \
             WHEN MATCHED THEN UPDATE SET v = generate_series(1, 2)",
            "set-returning functions are not allowed in UPDATE",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON t.id = s.x \
             WHEN NOT MATCHED THEN INSERT VALUES (5, count(*), 'x')",
            "aggregate functions are not allowed in VALUES",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON t.id = s.x \
             WHEN NOT MATCHED THEN INSERT VALUES (5, sum(1) OVER (), 'x')",
            "window functions are not allowed in VALUES",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON count(*) > 1 WHEN MATCHED THEN DELETE",
            "aggregate functions are not allowed in JOIN conditions",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON sum(1) OVER () > 1 WHEN MATCHED THEN DELETE",
            "window functions are not allowed in JOIN conditions",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON generate_series(1, 2) > 1 \
             WHEN MATCHED THEN DELETE",
            "set-returning functions are not allowed in JOIN conditions",
        ),
        (
            "UPDATE t SET v = 5 RETURNING count(*)",
            "aggregate functions are not allowed in RETURNING",
        ),
        (
            "UPDATE t SET v = 5 RETURNING GROUPING(v)",
            "grouping operations are not allowed in RETURNING",
        ),
        (
            "DELETE FROM t RETURNING row_number() OVER ()",
            "window functions are not allowed in RETURNING",
        ),
        (
            "UPDATE t SET v = 5 RETURNING generate_series(1, 2)",
            "set-returning functions are not allowed in RETURNING",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON t.id = s.x \
             WHEN MATCHED THEN DELETE RETURNING count(*)",
            "aggregate functions are not allowed in RETURNING",
        ),
        (
            "MERGE INTO t USING (VALUES (1)) s(x) ON t.id = s.x \
             WHEN MATCHED THEN DELETE RETURNING generate_series(1, 2)",
            "set-returning functions are not allowed in RETURNING",
        ),
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
}

#[test]
fn allowed_placements() {
    let db = setup();
    for sql in [
        // A single-row VALUES is the INSERT's target list: SRFs are fine.
        "INSERT INTO t VALUES (1, generate_series(1, 2), 'x')",
        // Aggregates / windows inside a sub-select belong to it.
        "UPDATE t SET v = (SELECT count(*) FROM t) RETURNING (SELECT max(v) FROM t)",
        "INSERT INTO t VALUES (1, (SELECT count(*) FROM t)::int, 'x')",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn a_target_named_excluded_is_ambiguous_in_on_conflict() {
    // DO UPDATE's SET list and WHERE see the target and EXCLUDED: a target
    // that is itself named `excluded` makes the qualifier name both
    // (scanNameSpaceForRefname, 42P09) — a plain column, both columns.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE excluded (key int PRIMARY KEY, data text);")
        .unwrap();
    for sql in [
        "INSERT INTO excluded VALUES (1, '2')
         ON CONFLICT (key) DO UPDATE SET data = excluded.data RETURNING *",
        "INSERT INTO excluded VALUES (1, '2')
         ON CONFLICT (key) DO UPDATE SET data = 'x' WHERE excluded.key = 1",
        "INSERT INTO excluded VALUES (1, '2')
         ON CONFLICT (key) DO UPDATE SET data = (SELECT excluded.data)",
        "INSERT INTO excluded VALUES (1, '2')
         ON CONFLICT (key) DO UPDATE SET data = excluded",
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(
            matches!(err, AnalyzeError::AmbiguousAlias(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with(r#"table reference "excluded" is ambiguous"#),
            "{sql}\n  got: {err}"
        );
    }
    let err = db
        .analyze("INSERT INTO excluded VALUES (1, '2') ON CONFLICT (key) DO UPDATE SET data = data")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with(r#"column reference "data" is ambiguous"#),
        "{err}"
    );
    // RETURNING sees the target alone; an alias tells the two apart.
    for sql in [
        "INSERT INTO excluded VALUES (1, '2')
         ON CONFLICT (key) DO UPDATE SET data = 'x' RETURNING excluded.key, excluded.*",
        "INSERT INTO excluded AS e VALUES (1, '2')
         ON CONFLICT (key) DO UPDATE SET data = excluded.data WHERE e.key = 1 RETURNING e.*",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
