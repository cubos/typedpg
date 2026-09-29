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

// ── Set-operation ORDER BY / LIMIT ───────────────────────────────────────────

fn setup_wide() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, b text, c varchar(10) NOT NULL, d numeric(10,2));",
    )
    .unwrap();
    db
}

#[test]
fn set_operation_order_by_and_limit_are_analyzed() {
    let db = setup_wide();
    let s = db
        .analyze(
            "SELECT * FROM t WHERE id = 1 UNION ALL SELECT * FROM t WHERE id = 2 \
             ORDER BY id LIMIT $l",
        )
        .unwrap();
    assert_params(&s, vec![p(int8())]);
    let s = db
        .analyze("SELECT a FROM t UNION SELECT a FROM t ORDER BY 1 OFFSET $o")
        .unwrap();
    assert_params(&s, vec![p(int8())]);
    db.analyze("SELECT a AS z FROM t UNION SELECT a FROM t ORDER BY z DESC NULLS LAST")
        .unwrap();
}

#[test]
fn set_operation_order_by_errors() {
    let db = setup_wide();
    let err = assert_err_prefix(
        &db,
        "SELECT a FROM t UNION SELECT a FROM t ORDER BY b",
        "column \"b\" does not exist",
    );
    assert!(matches!(err, AnalyzeError::UndefinedColumn(_)), "{err:?}");
    let err = assert_err_prefix(
        &db,
        "SELECT a FROM t UNION SELECT a FROM t ORDER BY a + 1",
        "invalid UNION/INTERSECT/EXCEPT ORDER BY clause",
    );
    assert!(
        matches!(err, AnalyzeError::FeatureNotSupported(_)),
        "{err:?}"
    );
    let err = assert_err_prefix(
        &db,
        "SELECT a FROM t UNION SELECT a FROM t ORDER BY t.a",
        "missing FROM-clause entry for table \"t\"",
    );
    assert!(matches!(err, AnalyzeError::UndefinedTable(_)), "{err:?}");
    let err = assert_err_prefix(
        &db,
        "SELECT a FROM t UNION SELECT a FROM t ORDER BY 2",
        "ORDER BY position 2 is not in select list",
    );
    assert!(
        matches!(err, AnalyzeError::InvalidColumnReference(_)),
        "{err:?}"
    );
    let err = assert_err_prefix(
        &db,
        "SELECT a FROM t UNION SELECT a FROM t LIMIT a",
        "column \"a\" does not exist",
    );
    assert!(matches!(err, AnalyzeError::UndefinedColumn(_)), "{err:?}");
}

// ── DISTINCT ON / ORDER BY USING / LIMIT / DEFAULT ───────────────────────────

#[test]
fn distinct_on_must_match_leading_order_by() {
    let db = setup_wide();
    for sql in [
        "SELECT DISTINCT ON (a) a, b FROM t ORDER BY b",
        "SELECT DISTINCT ON (a, b) a, b FROM t ORDER BY a, id, b",
    ] {
        let err = assert_err_prefix(
            &db,
            sql,
            "SELECT DISTINCT ON expressions must match initial ORDER BY expressions",
        );
        assert!(
            matches!(err, AnalyzeError::InvalidColumnReference(_)),
            "{sql}: {err:?}"
        );
    }
    for sql in [
        "SELECT DISTINCT ON (a) a, b FROM t ORDER BY a, b",
        "SELECT DISTINCT ON (a, b) a, b FROM t ORDER BY b, a, id",
        "SELECT DISTINCT ON (a, b) a, b FROM t ORDER BY a",
        "SELECT DISTINCT ON (1) a, b FROM t ORDER BY a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn order_by_using_needs_an_ordering_operator() {
    let db = setup_wide();
    let err = assert_err_prefix(
        &db,
        "SELECT a FROM t ORDER BY a USING @@@",
        "operator does not exist: integer @@@ integer",
    );
    assert!(matches!(err, AnalyzeError::UndefinedOperator(_)), "{err:?}");
    let err = assert_err_prefix(
        &db,
        "SELECT a FROM t ORDER BY a USING =",
        "operator = is not a valid ordering operator",
    );
    assert!(matches!(err, AnalyzeError::WrongObjectType(_)), "{err:?}");
    db.analyze("SELECT a FROM t ORDER BY a USING <").unwrap();
    db.analyze("SELECT b FROM t ORDER BY b USING ~<~").unwrap();
}

#[test]
fn limit_must_not_contain_variables() {
    let db = setup_wide();
    let err = assert_err_prefix(
        &db,
        "SELECT a FROM t LIMIT a",
        "argument of LIMIT must not contain variables",
    );
    assert!(
        matches!(err, AnalyzeError::InvalidColumnReference(_)),
        "{err:?}"
    );
    db.analyze("SELECT a FROM t LIMIT (SELECT max(a) FROM t)")
        .unwrap();
}

#[test]
fn default_outside_insert_is_rejected() {
    let db = setup_wide();
    for sql in ["VALUES (DEFAULT)", "SELECT DEFAULT"] {
        let err = assert_err_prefix(&db, sql, "DEFAULT is not allowed in this context");
        assert!(
            matches!(err, AnalyzeError::SyntaxError(_)),
            "{sql}: {err:?}"
        );
    }
}

#[test]
fn default_nested_in_an_expression_is_rejected() {
    // Only a bare DEFAULT (parentheses allowed) is an assignment's default;
    // inside a larger expression it is PG's parse-time 42601.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE d (id int PRIMARY KEY, x int);")
        .unwrap();
    for sql in [
        "INSERT INTO d VALUES (DEFAULT + 1, 1)",
        "UPDATE d SET x = DEFAULT + 1",
    ] {
        let err = assert_err_prefix(&db, sql, "DEFAULT is not allowed in this context");
        assert!(
            matches!(err, AnalyzeError::SyntaxError(_)),
            "{sql}: {err:?}"
        );
    }
    db.analyze("INSERT INTO d VALUES (1, (DEFAULT))").unwrap();
}

// ── SELECT INTO ──────────────────────────────────────────────────────────────

/// `SELECT … INTO` is CREATE TABLE AS: no result rows.
#[test]
fn select_into_returns_no_rows() {
    let db = setup_wide();
    let s = db.analyze("SELECT * INTO newt FROM t").unwrap();
    assert_cols(&s, vec![]);
    let s = db.analyze("SELECT 'x' INTO TEMP tt").unwrap();
    assert_cols(&s, vec![]);
    assert!(!s.can_run_as_subquery);
}

// ── Unknown-typed outputs ────────────────────────────────────────────────────

/// PG coerces a set-operation arm's untyped literal to the other arm's type.
#[test]
fn union_coerces_an_unknown_literal_to_the_other_arm() {
    let db = setup_wide();
    let err = assert_err_prefix(
        &db,
        "SELECT 1 UNION SELECT 'x'",
        "invalid input syntax for type integer: \"x\"",
    );
    assert!(matches!(err, AnalyzeError::InvalidLiteral(_)), "{err:?}");
    let s = db.analyze("SELECT 1 UNION SELECT '2'").unwrap();
    assert_cols(&s, vec![c("?column?", int4())]);
}

#[test]
fn values_keeps_a_common_typmod() {
    let db = setup_wide();
    let s = db
        .analyze("VALUES ('x'::varchar(3)), ('yy'::varchar(3))")
        .unwrap();
    assert_cols(&s, vec![c("column1", varchar_n(3))]);
}

/// A sub-SELECT's untyped parameter output resolves to text, so using it as
/// an integer fails like in PG (`operator does not exist: integer = text`).
#[test]
fn subquery_unknown_param_output_is_text() {
    let db = setup_wide();
    for (sql, msg) in [
        (
            "SELECT * FROM t WHERE id = (SELECT $p)",
            "operator does not exist: integer = text",
        ),
        (
            "SELECT (SELECT $p) + 1",
            "operator does not exist: text + integer",
        ),
        (
            "SELECT s.v + 1 FROM (SELECT $p AS v) s",
            "operator does not exist: text + integer",
        ),
        (
            "WITH w AS (SELECT $p AS v) SELECT v + 1 FROM w",
            "operator does not exist: text + integer",
        ),
    ] {
        let err = assert_err_prefix(&db, sql, msg);
        assert!(
            matches!(err, AnalyzeError::UndefinedOperator(_)),
            "{sql}: {err:?}"
        );
    }
    let s = db.analyze("SELECT (SELECT $p) AS v").unwrap();
    assert_params(&s, vec![p(text())]);
}

// ── Implicit column names ────────────────────────────────────────────────────

#[test]
fn grouping_function_column_is_named_grouping() {
    let db = setup_wide();
    let s = db
        .analyze("SELECT a, GROUPING(a) FROM t GROUP BY a")
        .unwrap();
    assert_eq!(s.columns[1].name, "grouping");
}

// ── TABLESAMPLE ──────────────────────────────────────────────────────────────

#[test]
fn tablesample_arguments_are_analyzed() {
    let db = setup();
    let s = db
        .analyze("SELECT * FROM t TABLESAMPLE SYSTEM ($p) REPEATABLE ($s)")
        .unwrap();
    assert_params(&s, vec![p(float4()), p(float8())]);
    let cases: &[(&str, &str)] = &[
        (
            "SELECT * FROM t TABLESAMPLE nosuch (10)",
            "tablesample method nosuch does not exist",
        ),
        (
            "SELECT * FROM t TABLESAMPLE SYSTEM ('x')",
            "invalid input syntax for type real: \"x\"",
        ),
        (
            "SELECT * FROM t TABLESAMPLE SYSTEM (1, 2)",
            "tablesample method system requires 1 argument, not 2",
        ),
        (
            "SELECT * FROM t TABLESAMPLE BERNOULLI (t.a)",
            "invalid reference to FROM-clause entry for table \"t\"",
        ),
        (
            "SELECT * FROM t TABLESAMPLE BERNOULLI (true)",
            "argument of TABLESAMPLE must be type real, not type boolean",
        ),
    ];
    for (sql, msg) in cases {
        assert_err_prefix(&db, sql, msg);
    }
}

// ── Ordinals over a star-expanded select list ────────────────────────────────

/// `findTargetlistEntrySQL92` counts the *expanded* target list: every
/// column a `*` / `t.*` / `(row).*` contributes is its own position.
#[test]
fn ordinals_count_star_expanded_targets() {
    let db = setup();
    for sql in [
        "SELECT tableoid::regclass, * FROM t ORDER BY 1, 2, 3, 4",
        "SELECT * FROM t GROUP BY 1, 2, 3",
        "SELECT (t).*, 1 FROM t ORDER BY 4",
        "SELECT DISTINCT ON (3) * FROM t ORDER BY 3",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (sql, msg) in [
        (
            "SELECT tableoid::regclass, t.* FROM t ORDER BY 5",
            "ORDER BY position 5 is not in select list",
        ),
        (
            "SELECT (t).*, 1 FROM t ORDER BY 5",
            "ORDER BY position 5 is not in select list",
        ),
        (
            "SELECT DISTINCT ON (5) * FROM t",
            "DISTINCT ON position 5 is not in select list",
        ),
        (
            "SELECT DISTINCT ON (2) a FROM t",
            "DISTINCT ON position 2 is not in select list",
        ),
    ] {
        let err = assert_err_prefix(&db, sql, msg);
        assert!(
            matches!(err, AnalyzeError::InvalidColumnReference(_)),
            "{err:?}"
        );
    }
    // The ordinal lands on the aggregate past the star's columns.
    let err = assert_err_prefix(
        &db,
        "SELECT *, count(*) FROM t GROUP BY 1, 4",
        "aggregate functions are not allowed in GROUP BY",
    );
    assert!(matches!(err, AnalyzeError::GroupingError(_)), "{err:?}");
}
