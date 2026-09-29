//! MERGE: per-WHEN-clause visibility of target and source, RETURNING over
//! both relations, `merge_action()`, and MERGE as a CTE body.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL, w text);
         CREATE TABLE s (id int PRIMARY KEY, v int NOT NULL);",
    )
    .unwrap();
    db
}

// ── WHEN-clause visibility (PG's setNamespaceForMergeWhen) ───────────────────

#[test]
fn not_matched_by_source_cannot_see_the_source() {
    let db = setup();
    let err = db
        .analyze(
            "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED BY SOURCE THEN UPDATE SET v = s.v",
        )
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::UndefinedTable(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("invalid reference to FROM-clause entry for table \"s\""),
        "{err}"
    );
    let err = db
        .analyze(
            "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED BY SOURCE AND s.v > 0 THEN DELETE",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("invalid reference to FROM-clause entry for table \"s\""),
        "{err}"
    );
}

#[test]
fn not_matched_by_target_cannot_see_the_target() {
    let db = setup();
    for sql in [
        "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (t.id, 1)",
        "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED AND t.v > 0 THEN INSERT VALUES (1, 1)",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(matches!(err, AnalyzeError::UndefinedTable(_)), "{err:?}");
        assert!(
            err.to_string()
                .starts_with("invalid reference to FROM-clause entry for table \"t\""),
            "{sql}: {err}"
        );
    }
}

/// An unqualified name resolves against the visible side only — `v` is
/// ambiguous under WHEN MATCHED but not in the one-sided arms.
#[test]
fn unqualified_names_resolve_against_the_visible_side() {
    let db = setup();
    db.analyze("MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id, v)")
        .unwrap();
    db.analyze(
        "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED BY SOURCE THEN UPDATE SET w = id::text",
    )
    .unwrap();
    let err = db
        .analyze("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET v = v + 1")
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::AmbiguousColumn(_)), "{err:?}");
}

// ── RETURNING ────────────────────────────────────────────────────────────────

#[test]
fn returning_sees_the_source() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE \
             RETURNING s.id AS sid, s.v AS sv, t.v AS tv",
        )
        .unwrap();
    assert_cols(&s, vec![c("sid", int4()), c("sv", int4()), c("tv", int4())]);
}

/// PG 18: `RETURNING *` is the source's columns, then the target's.
#[test]
fn returning_star_lists_source_then_target() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v RETURNING *",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int4()),
            c("v", int4()),
            c("id", int4()),
            c("v", int4()),
            cn("w", text()),
        ],
    );
    let s = db
        .analyze(
            "MERGE INTO t USING (SELECT * FROM s) ss ON t.id = ss.id WHEN MATCHED THEN DELETE \
             RETURNING ss.*, t.id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int4()), c("v", int4()), c("id", int4())]);
}

/// A WHEN NOT MATCHED BY SOURCE action returns rows whose source side is
/// NULL.
#[test]
fn returning_source_is_nullable_with_not_matched_by_source() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED BY SOURCE THEN DELETE \
             RETURNING s.v AS sv, t.v AS tv",
        )
        .unwrap();
    assert_cols(&s, vec![cn("sv", int4()), c("tv", int4())]);
}

// ── merge_action() ───────────────────────────────────────────────────────────

#[test]
fn merge_action_in_returning() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, 0) \
             RETURNING merge_action() AS act, t.*",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("act", text()),
            c("id", int4()),
            c("v", int4()),
            cn("w", text()),
        ],
    );
    // A sublink inside RETURNING may use it too (a subquery without FROM
    // yields exactly one row, so it is as NOT NULL as merge_action()).
    let s = db
        .analyze(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE \
             RETURNING (SELECT merge_action())",
        )
        .unwrap();
    assert_cols(&s, vec![c("merge_action", text())]);
}

#[test]
fn merge_action_outside_merge_returning_is_rejected() {
    let db = setup();
    for sql in [
        "SELECT merge_action()",
        "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED AND merge_action() = 'x' THEN DELETE",
        "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET w = (SELECT merge_action())",
        "MERGE INTO t USING s ON t.id = s.id AND merge_action() = 'x' WHEN MATCHED THEN DELETE",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(matches!(err, AnalyzeError::SyntaxError(_)), "{err:?}");
        assert!(
            err.to_string().starts_with(
                "MERGE_ACTION() can only be used in the RETURNING list of a MERGE command"
            ),
            "{sql}: {err}"
        );
    }
}

// ── MERGE as a CTE body ──────────────────────────────────────────────────────

#[test]
fn merge_returning_as_cte_body() {
    let db = setup();
    let s = db
        .analyze(
            "WITH m AS (MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v \
             RETURNING t.id, merge_action() AS a) SELECT * FROM m",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int4()), c("a", text())]);
}

// ── transformMergeStmt's structural rules ────────────────────────────────────

/// transformInsertRow's arity rule applies to a WHEN NOT MATCHED INSERT.
#[test]
fn merge_insert_values_must_match_the_target_columns() {
    let db = setup();
    let err = db
        .analyze(
            "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT (id, v) VALUES (1)",
        )
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::SyntaxError(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("INSERT has more target columns than expressions"),
        "{err}"
    );
    let err = db
        .analyze(
            "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (1, 2, 'x', 4)",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("INSERT has more expressions than target columns"),
        "{err}"
    );
    // Without a column list, the missing trailing columns take defaults.
    db.analyze("MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id, 1)")
        .unwrap();
    db.analyze("MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT DEFAULT VALUES")
        .unwrap();
}

/// A WHEN clause after an unconditional one of the same match kind can
/// never run; other match kinds are unaffected.
#[test]
fn merge_rejects_unreachable_when_clauses() {
    let db = setup();
    for sql in [
        "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET v = 1 \
         WHEN MATCHED THEN DELETE",
        "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED BY SOURCE THEN DELETE \
         WHEN NOT MATCHED BY SOURCE AND t.v > 0 THEN DO NOTHING",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::SyntaxError(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("unreachable WHEN clause specified after unconditional WHEN clause"),
            "{sql}: {err}"
        );
    }
    db.analyze(
        "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED AND s.v > 0 THEN DELETE \
         WHEN MATCHED THEN DO NOTHING WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE",
    )
    .unwrap();
}

/// The target and the data source may not share a name.
#[test]
fn merge_target_and_source_need_distinct_names() {
    let db = setup();
    for sql in [
        "MERGE INTO t USING t ON true WHEN MATCHED THEN DELETE",
        "MERGE INTO t USING s AS t ON true WHEN MATCHED THEN DELETE",
        "MERGE INTO t AS x USING (SELECT 1 AS id) x ON true WHEN MATCHED THEN DELETE",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::DuplicateAlias(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string().starts_with("name \"")
                && err.to_string().contains("\" specified more than once"),
            "{sql}: {err}"
        );
    }
    db.analyze("MERGE INTO t AS x USING t ON x.id = t.id WHEN MATCHED THEN DELETE")
        .unwrap();
}
