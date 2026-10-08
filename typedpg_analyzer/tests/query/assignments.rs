//! Assignment targets: multi-column `SET (a, b) = …`, subscripted / field
//! assignment, duplicate targets, INSERT target aliases and OVERRIDING.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, b text);
         CREATE TABLE u (id int PRIMARY KEY, t_id int NOT NULL, x text NOT NULL);
         CREATE TABLE k (a int NOT NULL, b int NOT NULL, c text, UNIQUE (a, b), CONSTRAINT k_c_key UNIQUE (c));
         CREATE TYPE pt AS (x int, y text);
         CREATE TABLE j (id int PRIMARY KEY, j jsonb NOT NULL, arr int[] NOT NULL, p pt, n int);
         CREATE TABLE g (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v int NOT NULL);",
    )
    .unwrap();
    db
}

// ── SET (a, b) = … ───────────────────────────────────────────────────────────

#[test]
fn multi_column_update_forms() {
    let db = setup();
    db.analyze("UPDATE t SET (a, b) = (SELECT t_id, x FROM u WHERE u.id = t.id)")
        .unwrap();
    db.analyze("UPDATE t SET (a, b) = ROW(1, 'x')").unwrap();
    db.analyze("UPDATE t SET (a, b) = (1, 'x')").unwrap();
    db.analyze("UPDATE t SET (a, b) = (DEFAULT, 'x')").unwrap();
    db.analyze(
        "INSERT INTO k VALUES (1,2,'x') ON CONFLICT (c) DO UPDATE SET (a, b) = (excluded.a, excluded.b)",
    )
    .unwrap();
    let s = db.analyze("UPDATE t SET (a, b) = ($x, $y)").unwrap();
    assert_params(&s, vec![p(int4()), pn(text())]);
}

#[test]
fn multi_column_update_errors() {
    let db = setup();
    for sql in [
        "UPDATE t SET (a, b) = (1, 'x', 3)",
        "UPDATE t SET (a, b) = (SELECT 1)",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::SyntaxError(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("number of columns does not match number of values"),
            "{sql}: {err}"
        );
    }
    let err = db
        .analyze("UPDATE t SET (a, b) = (SELECT 'x'::text, 'y')")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("column \"a\" is of type integer but expression is of type text"),
        "{err}"
    );
    let err = db.analyze("UPDATE t SET (a) = (5)").unwrap_err();
    assert!(
        matches!(err, AnalyzeError::FeatureNotSupported(_)),
        "{err:?}"
    );
    assert!(
        err.to_string().starts_with(
            "source for a multiple-column UPDATE item must be a sub-SELECT or ROW() expression"
        ),
        "{err}"
    );
}

// ── Subscripted / field assignment ───────────────────────────────────────────

#[test]
fn subscripted_assignment_targets_the_element() {
    let db = setup();
    db.analyze("UPDATE j SET arr[1] = 5").unwrap();
    let s = db.analyze("UPDATE j SET arr[1] = $x").unwrap();
    assert_params(&s, vec![pn(int4())]);
    db.analyze("INSERT INTO j (id, j, arr[1]) VALUES (1, '{}', 5)")
        .unwrap();
    db.analyze("UPDATE j SET j['a'] = NULL").unwrap();
    let s = db.analyze("UPDATE j SET j[$k] = '1'").unwrap();
    assert_params(&s, vec![p(text())]);
    let s = db.analyze("UPDATE j SET arr[$i] = $v").unwrap();
    assert_params(&s, vec![p(int4()), pn(int4())]);
    let s = db.analyze("UPDATE j SET arr[1:2] = $v").unwrap();
    assert_params(&s, vec![pn(array_of(int4()))]);
    let s = db.analyze("UPDATE j SET p.x = $x, p.y = $y").unwrap();
    assert_params(&s, vec![pn(int4()), pn(text())]);
    db.analyze("UPDATE j SET arr[1] = 1, arr[2] = 2").unwrap();
    db.analyze("INSERT INTO j (id, j, arr[1], arr[2]) VALUES (1, '{}', 5, 6)")
        .unwrap();
}

#[test]
fn subscripted_assignment_errors() {
    let db = setup();
    let cases: &[(&str, &str)] = &[
        (
            "UPDATE j SET j['a'] = 5",
            "subscripted assignment to \"j\" requires type jsonb but expression is of type integer",
        ),
        (
            "UPDATE j SET arr[1] = 'x'::text",
            "subscripted assignment to \"arr\" requires type integer but expression is of type text",
        ),
        (
            "UPDATE j SET p.x = 'x'::text",
            "subfield \"x\" is of type integer but expression is of type text",
        ),
        (
            "UPDATE j SET n[1] = 1",
            "cannot subscript type integer because it does not support subscripting",
        ),
        (
            "UPDATE t SET a.foo = 1",
            "cannot assign to field \"foo\" of column \"a\" because its type integer is not a composite type",
        ),
    ];
    for (sql, msg) in cases {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::DatatypeMismatch(_)),
            "{sql}: {err:?}"
        );
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
    let err = db.analyze("UPDATE j SET p.z = 1").unwrap_err();
    assert!(matches!(err, AnalyzeError::UndefinedColumn(_)), "{err:?}");
    assert!(
        err.to_string().starts_with(
            "cannot assign to field \"z\" of column \"p\" because there is no such column in data type pt"
        ),
        "{err}"
    );
}

#[test]
fn assignment_mismatch_points_at_the_value_with_pg_hint() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("UPDATE j SET arr[1] = 'x'::text"),
        AnalyzeError::DatatypeMismatch(_),
        "\
subscripted assignment to \"arr\" requires type integer but expression is of type text
  ╭────
1 │ UPDATE j SET arr[1] = 'x'::text
  ·                          ┬
  ·                          ╰─ expected integer, found text
  ╰────
  help: You will need to rewrite or cast the expression.
  note: an explicit cast from text to integer exists: `expr::integer`
"
    );
    // A subquery's column has no node of its own: the target column is
    // marked.
    assert_analyze_err!(
        db.analyze("UPDATE j SET (n, arr) = (SELECT 1, 'x'::text)"),
        AnalyzeError::DatatypeMismatch(_),
        "\
column \"arr\" is of type integer[] but expression is of type text
  ╭────
1 │ UPDATE j SET (n, arr) = (SELECT 1, 'x'::text)
  ·                  ─┬─
  ·                   ╰─ expected integer[], found text
  ╰────
  help: You will need to rewrite or cast the expression.
  note: an explicit cast from text to integer[] exists: `expr::integer[]`
"
    );
}

// ── Duplicate targets ────────────────────────────────────────────────────────

#[test]
fn duplicate_insert_target_column() {
    let db = setup();
    for sql in [
        "INSERT INTO t (id, id, a) VALUES (1, 2, 3)",
        "INSERT INTO j (id, j, arr[1], arr) VALUES (1, '{}', 5, '{}')",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::DuplicateColumn(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string().contains("specified more than once"),
            "{sql}: {err}"
        );
    }
}

#[test]
fn multiple_assignments_to_same_column() {
    let db = setup();
    for (sql, col) in [
        ("UPDATE t SET a = 1, a = 2", "a"),
        ("UPDATE t SET (a, a) = (1, 2)", "a"),
        ("UPDATE j SET arr[1] = 1, arr = '{}'", "arr"),
    ] {
        assert_analyze_err!(
            db.analyze(sql),
            AnalyzeError::SyntaxError(_),
            &format!("multiple assignments to same column \"{col}\"")
        );
    }
}

// ── INSERT target alias / OVERRIDING ─────────────────────────────────────────

#[test]
fn insert_target_alias_in_on_conflict_and_returning() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO k AS kk (a, b, c) VALUES (1, 2, 'x') \
             ON CONFLICT (a, b) DO UPDATE SET c = kk.c || excluded.c RETURNING kk.a",
        )
        .unwrap();
    assert_cols(&s, vec![c("a", int4())]);
}

#[test]
fn overriding_user_value_on_generated_always_identity() {
    let db = setup();
    db.analyze("INSERT INTO g (id, v) OVERRIDING USER VALUE VALUES (1, 2)")
        .unwrap();
    db.analyze("INSERT INTO g (id, v) OVERRIDING USER VALUE SELECT 1, 2")
        .unwrap();
}

// ── INSERT … SELECT arity ────────────────────────────────────────────────────

/// The arity check counts the SELECT's *output* columns (after `*`
/// expansion and set operations), not its raw target entries.
#[test]
fn insert_select_arity_uses_output_columns() {
    let db = setup();
    for sql in [
        "INSERT INTO t (id, a, b) SELECT 1, 2, 'x' UNION ALL SELECT 2, 3, 'y'",
        "INSERT INTO t SELECT * FROM t",
        "INSERT INTO t (id, a, b) SELECT * FROM (VALUES (1, 2, 'x')) v",
        "INSERT INTO t (id, a, b) SELECT v.* FROM (VALUES (1, 2, 'x')) v",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let err = db
        .analyze("INSERT INTO t SELECT u.*, 1 FROM u")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("INSERT has more expressions than target columns"),
        "{err}"
    );
}

// ── ON CONFLICT arbiter inference ────────────────────────────────────────────

fn setup_arbiters() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE k (a int NOT NULL, b int NOT NULL, c text, UNIQUE (a, b), CONSTRAINT k_c_key UNIQUE (c));
         CREATE UNIQUE INDEX k_part ON k (a) WHERE c IS NULL;
         CREATE UNIQUE INDEX k_expr ON k (lower(c));",
    )
    .unwrap();
    db
}

/// PG's infer_arbiter_indexes: a partial unique index is an arbiter when the
/// ON CONFLICT WHERE implies its predicate; an expression index matches the
/// same inference expression.
#[test]
fn on_conflict_infers_partial_and_expression_indexes() {
    let db = setup_arbiters();
    for sql in [
        "INSERT INTO k (a,b,c) VALUES (1,2,'x') ON CONFLICT (a) WHERE c IS NULL DO NOTHING",
        "INSERT INTO k (a,b,c) VALUES (1,2,'x') ON CONFLICT (a) WHERE c IS NULL AND b > 0 DO NOTHING",
        "INSERT INTO k (a,b,c) VALUES (1,2,'x') ON CONFLICT (lower(c)) DO NOTHING",
        "INSERT INTO k (a,b,c) VALUES (1,2,'x') ON CONFLICT (a, b) WHERE b > 0 DO NOTHING",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for sql in [
        "INSERT INTO k (a,b,c) VALUES (1,2,'x') ON CONFLICT (a) DO NOTHING",
        "INSERT INTO k (a,b,c) VALUES (1,2,'x') ON CONFLICT (upper(c)) DO NOTHING",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::InvalidColumnReference(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string().starts_with(
                "there is no unique or exclusion constraint matching the ON CONFLICT specification"
            ),
            "{sql}: {err}"
        );
    }
}

/// infer_collation_opclass_match: an element's explicit COLLATE must be
/// the index column's collation, and its operator class must share the
/// index column's operator family and input type (text_pattern_ops matches
/// a varchar_pattern_ops index, varchar_ops a varchar one only through
/// text_ops).
#[test]
fn on_conflict_matches_explicit_collations_and_operator_classes() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (a int, b text, c varchar, d text);
         CREATE UNIQUE INDEX ON t (b);
         CREATE UNIQUE INDEX ON t (c varchar_pattern_ops);
         CREATE UNIQUE INDEX ON t ((a + 1));
         CREATE UNIQUE INDEX ON t (d COLLATE \"C\");
         CREATE VIEW v AS SELECT b AS vb FROM t;",
    )
    .unwrap();
    for target in [
        "(b COLLATE \"default\")",
        "(b text_ops)",
        "(c text_pattern_ops)",
        "(c varchar_pattern_ops)",
        "(c)",
        "((a + 1) int4_ops)",
        "(d)",
        "(d COLLATE \"C\")",
        "(d COLLATE \"C\" text_ops)",
    ] {
        let sql =
            format!("INSERT INTO t VALUES (1, 'x', 'y', 'z') ON CONFLICT {target} DO NOTHING");
        db.analyze(&sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    db.analyze("INSERT INTO v VALUES ('x') ON CONFLICT (vb text_ops) DO NOTHING")
        .unwrap();
    let mut rejected: Vec<String> = [
        "(b COLLATE \"C\")",
        "(b text_pattern_ops)",
        "(c varchar_ops)",
        "((a + 1) int8_ops)",
        "((a + 1) COLLATE \"C\")",
        "(d COLLATE \"POSIX\")",
    ]
    .iter()
    .map(|target| {
        format!("INSERT INTO t VALUES (1, 'x', 'y', 'z') ON CONFLICT {target} DO NOTHING")
    })
    .collect();
    rejected.push("INSERT INTO v VALUES ('x') ON CONFLICT (vb COLLATE \"C\") DO NOTHING".into());
    for sql in &rejected {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            err.to_string().starts_with(
                "there is no unique or exclusion constraint matching the ON CONFLICT specification"
            ),
            "{sql}: {err}"
        );
    }
}

fn setup_inference() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL, w text, z int);
         CREATE UNIQUE INDEX t_z_expr ON t ((z + 1));
         CREATE UNIQUE INDEX t_w_part ON t (w) WHERE v > 0;",
    )
    .unwrap();
    db
}

/// transformOnConflictArbiter transforms the inference WHERE as an index
/// predicate against the target alone: parameters are typed, bad columns,
/// aggregates, SRFs, sub-selects and EXCLUDED are rejected.
#[test]
fn on_conflict_inference_where_is_analyzed() {
    let db = setup_inference();
    let s = db
        .analyze("INSERT INTO t (id, v) VALUES (1, 1) ON CONFLICT (id) WHERE id > $a DO NOTHING")
        .unwrap();
    assert_params(&s, vec![p(int4())]);
    let cases: &[(&str, &str)] = &[
        ("WHERE nope > 0", "column \"nope\" does not exist"),
        (
            "WHERE sum(1) > 0",
            "aggregate functions are not allowed in index predicates",
        ),
        (
            "WHERE generate_series(1, 2) > 0",
            "set-returning functions are not allowed in index predicates",
        ),
        (
            "WHERE (SELECT true)",
            "cannot use subquery in index predicate",
        ),
        (
            "WHERE excluded.v > 0",
            "missing FROM-clause entry for table \"excluded\"",
        ),
    ];
    for (clause, msg) in cases {
        let sql =
            format!("INSERT INTO t (id, v) VALUES (1, 1) ON CONFLICT (id) {clause} DO NOTHING");
        let err = db.analyze(&sql).unwrap_err();
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
    // Under DO UPDATE, EXCLUDED is in the range table but not referencable.
    let err = db
        .analyze(
            "INSERT INTO t (id, v) VALUES (1, 1) ON CONFLICT (id) WHERE excluded.v > 0 \
             DO UPDATE SET v = 2",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("invalid reference to FROM-clause entry for table \"excluded\""),
        "{err}"
    );
}

/// infer_arbiter_indexes accepts a partial index whose predicate the ON
/// CONFLICT WHERE implies (predicate_implied_by), not only a verbatim copy.
#[test]
fn on_conflict_partial_index_predicate_is_implied() {
    let db = setup_inference();
    for clause in [
        "WHERE v > 0",
        "WHERE v > 1",
        "WHERE 0 < v",
        "WHERE v >= 1",
        "WHERE v = 5",
        "WHERE t.v > 0 AND w IS NOT NULL",
        "WHERE (v > 3 OR v = 1)",
    ] {
        let sql =
            format!("INSERT INTO t (id, v) VALUES (1, 1) ON CONFLICT (w) {clause} DO NOTHING");
        db.analyze(&sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for clause in [
        "",
        "WHERE v >= 0",
        "WHERE v < 5",
        "WHERE v <> 0",
        "WHERE v > 0 OR id > 0",
    ] {
        let sql =
            format!("INSERT INTO t (id, v) VALUES (1, 1) ON CONFLICT (w) {clause} DO NOTHING");
        let err = db.analyze(&sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::InvalidColumnReference(_)),
            "{sql}: {err:?}"
        );
    }
}

/// predicate_classify: a ScalarArrayOpExpr over an array of elements reads
/// as the AND (`op ALL`, `NOT IN`) or OR (`op ANY`, `IN`) of its comparisons,
/// so `NOT IN ('a', 'b')` and pg_dump's `<> ALL (ARRAY['a'::text, ...])`
/// prove each other. A constant cast to its column's own type is the bare
/// literal.
#[test]
fn on_conflict_partial_index_predicate_over_a_list_is_implied() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE m (k int, status text NOT NULL, v int, w int);
         CREATE UNIQUE INDEX m_live ON m (k) WHERE (status <> ALL (ARRAY['revoked'::text, 'expired'::text]));
         CREATE UNIQUE INDEX m_v ON m (v) WHERE v = ANY (ARRAY[1, 2]);
         CREATE UNIQUE INDEX m_w ON m (w) WHERE w::bigint <> ALL (ARRAY[1::bigint]);",
    )
    .unwrap();
    let insert = |target: &str, clause: &str| {
        format!(
            "INSERT INTO m (k, status, v) VALUES (1, 'active', 1) ON CONFLICT ({target}) {clause} DO NOTHING"
        )
    };
    for (target, clause) in [
        ("k", "WHERE status NOT IN ('revoked', 'expired')"),
        ("k", "WHERE status NOT IN ('expired', 'revoked')"),
        ("k", "WHERE status NOT IN ('revoked', 'other', 'expired')"),
        ("k", "WHERE status <> ALL (ARRAY['revoked', 'expired'])"),
        ("k", "WHERE m.status = 'active'"),
        ("k", "WHERE status IN ('active', 'pending')"),
        ("v", "WHERE v IN (1)"),
        ("v", "WHERE v = ANY (ARRAY[2, 1])"),
        // The constant is cast to the type of `w::bigint`, its operand.
        ("w", "WHERE w::bigint <> 1"),
    ] {
        let sql = insert(target, clause);
        db.analyze(&sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (target, clause) in [
        ("k", "WHERE status NOT IN ('revoked')"),
        ("k", "WHERE status <> 'revoked'"),
        ("k", "WHERE status IN ('active', 'revoked')"),
        ("v", "WHERE v IN (1, 3)"),
        // `w`, not `w::bigint`: another operand.
        ("w", "WHERE w NOT IN (1)"),
    ] {
        let sql = insert(target, clause);
        let err = db.analyze(&sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::InvalidColumnReference(_)),
            "{sql}: {err:?}"
        );
    }
}

/// A comparison is matched as PG types it: `c <> 'a'::text` on a citext
/// column compares texts, `c::text <> 'a'::text` with the cast PG adds, so
/// a text comparison written with the cast proves it and `c NOT IN ('a')`,
/// a citext comparison, doesn't. A cast to the operand's own type is no
/// cast, nor binary-compatible ones that net out to nothing; another cast
/// makes another operand.
#[test]
fn on_conflict_partial_index_predicate_is_matched_as_typed() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE EXTENSION citext;
         CREATE TABLE m (k int, c citext, t text, v varchar);
         CREATE UNIQUE INDEX m_c ON m (k) WHERE c <> ALL (ARRAY['a'::text]);
         CREATE UNIQUE INDEX m_t ON m (t) WHERE t <> 'x';
         CREATE UNIQUE INDEX m_v ON m (v) WHERE v <> 'y';",
    )
    .unwrap();
    for (target, clause) in [
        ("k", "WHERE c::text <> 'a'"),
        ("k", "WHERE c::pg_catalog.text NOT IN ('a'::text, 'b')"),
        ("t", "WHERE t::text <> 'x'"),
        ("t", "WHERE t <> 'x'::text"),
        ("v", "WHERE v::text <> 'y'"),
        // Binary-compatible relabelings that net out to nothing.
        ("t", "WHERE t::varchar <> 'x'"),
        ("t", "WHERE t::varchar::text <> 'x'"),
    ] {
        let sql = format!(
            "INSERT INTO m ({target}) VALUES (NULL) ON CONFLICT ({target}) {clause} DO NOTHING"
        );
        db.analyze(&sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    for (target, clause) in [
        ("k", "WHERE c NOT IN ('a')"),
        ("k", "WHERE c <> 'a'::citext"),
        ("t", "WHERE t::char(1) <> 'x'"),
        ("t", "WHERE t::name <> 'x'"),
    ] {
        let sql = format!(
            "INSERT INTO m ({target}) VALUES (NULL) ON CONFLICT ({target}) {clause} DO NOTHING"
        );
        let err = db.analyze(&sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::InvalidColumnReference(_)),
            "{sql}: {err:?}"
        );
    }
}

/// resolve_unique_index_expr: no ordering options, index expressions are
/// transformed (so their errors are PG's), operator classes must exist,
/// and a system column is a valid (never matching) element.
#[test]
fn on_conflict_inference_elements() {
    let db = setup_inference();
    let cases: &[(&str, &str)] = &[
        ("(id DESC)", "ASC/DESC is not allowed in ON CONFLICT clause"),
        (
            "(id NULLS FIRST)",
            "NULLS FIRST/LAST is not allowed in ON CONFLICT clause",
        ),
        (
            "(w nope_ops)",
            "operator class \"nope_ops\" does not exist for access method \"btree\"",
        ),
        ("((nope + 1))", "column \"nope\" does not exist"),
        (
            "((id + 'x'))",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "(ctid)",
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        ),
        (
            "(t)",
            "whole row unique index inference specifications are not supported",
        ),
    ];
    for (target, msg) in cases {
        let sql = format!("INSERT INTO t (id, v) VALUES (1, 1) ON CONFLICT {target} DO NOTHING");
        let err = db.analyze(&sql).unwrap_err();
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
    db.analyze("INSERT INTO t (id, v) VALUES (1, 1) ON CONFLICT ((z + 1)) DO NOTHING")
        .unwrap();
    db.analyze("INSERT INTO t (id, v) VALUES (1, 1) ON CONFLICT (id int4_ops) DO NOTHING")
        .unwrap();
}

#[test]
fn on_conflict_do_update_requires_a_target() {
    let db = setup_arbiters();
    let err = db
        .analyze("INSERT INTO k (a,b,c) VALUES (1,2,'x') ON CONFLICT DO UPDATE SET a = 1")
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::SyntaxError(_)), "{err:?}");
    assert!(
        err.to_string().starts_with(
            "ON CONFLICT DO UPDATE requires inference specification or constraint name"
        ),
        "{err}"
    );
}

// ── WHERE CURRENT OF ─────────────────────────────────────────────────────────

#[test]
fn where_current_of_cursor() {
    let db = setup();
    let s = db.analyze("DELETE FROM t WHERE CURRENT OF cur").unwrap();
    assert_cols(&s, vec![]);
    let s = db
        .analyze("UPDATE t SET a = 1 WHERE CURRENT OF cur RETURNING *")
        .unwrap();
    assert_cols(&s, vec![c("id", int4()), c("a", int4()), cn("b", text())]);
}

// ── System columns ───────────────────────────────────────────────────────────

/// transformUpdateTargetList finds a system column (`attnameAttNum` with
/// `sysColOK`) and transformAssignedExpr refuses it (0A000) — in UPDATE,
/// ON CONFLICT DO UPDATE and MERGE UPDATE alike.
#[test]
fn assigning_to_a_system_column_is_refused() {
    let db = setup();
    for sql in [
        "UPDATE t SET ctid = '(0,1)'",
        "INSERT INTO t (id, a) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET ctid = DEFAULT",
        "MERGE INTO t USING u ON t.id = u.id WHEN MATCHED THEN UPDATE SET xmin = '1'",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("cannot assign to system column"),
            "{sql}: {err}"
        );
    }
}

/// The EXCLUDED pseudo-relation is a composite-type RTE: it has no system
/// columns, while the target's stay reachable.
#[test]
fn excluded_has_no_system_columns() {
    let db = setup();
    db.analyze(
        "INSERT INTO t (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = excluded.a \
         WHERE t.ctid IS NOT NULL",
    )
    .unwrap();
    let err = db
        .analyze(
            "INSERT INTO t (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = excluded.a \
             WHERE excluded.ctid IS NULL",
        )
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::UndefinedColumn(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("column excluded.ctid does not exist"),
        "{err}"
    );
}
