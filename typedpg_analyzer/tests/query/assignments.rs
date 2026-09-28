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
