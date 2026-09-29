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

/// transformSetOperationTree refuses a locking clause on any member of a
/// set operation, parenthesized or not, recursive-CTE terms included.
#[test]
fn locking_clause_on_a_set_operation_member() {
    let db = setup();
    for sql in [
        "(SELECT id FROM t FOR UPDATE) UNION SELECT id FROM u",
        "SELECT id FROM t UNION (SELECT id FROM u FOR SHARE)",
        "SELECT id FROM t UNION (SELECT id FROM u UNION (SELECT id FROM t FOR UPDATE))",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM r WHERE n < 3 FOR UPDATE)) \
         SELECT * FROM r",
    ] {
        let err = assert_err_prefix(&db, sql, "FOR ");
        assert!(
            err.to_string()
                .contains("is not allowed with UNION/INTERSECT/EXCEPT"),
            "{sql}: {err}"
        );
    }
}

/// transformLockingClause: `FOR UPDATE OF` names FROM entries, never
/// schema-qualified relations (42601).
#[test]
fn locking_clause_names_are_unqualified() {
    let db = setup();
    let err = assert_err_prefix(
        &db,
        "SELECT * FROM t FOR UPDATE OF public.t",
        "FOR UPDATE must specify unqualified relation names",
    );
    assert!(matches!(err, AnalyzeError::SyntaxError(_)), "{err:?}");
}

/// A locking clause pushed into a view (rewriter) or subquery reaches its
/// query: grouping there fails in the planner's CheckSelectLocking, a
/// relation on an outer join's nullable side in make_outerjoininfo — every
/// execution fails, so the analyzer rejects them.
#[test]
fn locking_clause_through_views_and_subqueries() {
    let mut db = setup();
    db.apply_sql(
        "CREATE VIEW va AS SELECT count(*) AS c FROM t;
         CREATE VIEW vl AS SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id = t.id;
         CREATE VIEW vv AS SELECT * FROM vl;
         CREATE VIEW vp AS SELECT id, a FROM t;",
    )
    .unwrap();
    for (sql, msg) in [
        (
            "SELECT * FROM va FOR UPDATE",
            "FOR UPDATE is not allowed with aggregate functions",
        ),
        (
            "SELECT * FROM (SELECT * FROM va) q FOR SHARE",
            "FOR SHARE is not allowed with aggregate functions",
        ),
        (
            "SELECT * FROM vl FOR UPDATE",
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
        ),
        (
            "SELECT * FROM vv FOR UPDATE",
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
        ),
        (
            "SELECT * FROM (SELECT t.id FROM t LEFT JOIN u ON u.t_id = t.id) q FOR UPDATE",
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
        ),
        (
            "SELECT * FROM t LEFT JOIN (SELECT * FROM u) q ON q.t_id = t.id FOR UPDATE",
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
        ),
    ] {
        let err = assert_err_prefix(&db, sql, msg);
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
    }
    for sql in [
        "SELECT * FROM vp FOR UPDATE",
        "SELECT * FROM va, t FOR UPDATE OF t",
        "SELECT * FROM (SELECT * FROM vp) q FOR UPDATE",
        "SELECT * FROM t LEFT JOIN (SELECT * FROM u) q ON q.t_id = t.id FOR UPDATE OF t",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
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

/// transformFromClauseItem samples only plain / partitioned tables and
/// materialized views; the arguments are FROM-function expressions, where
/// aggregates are refused.
#[test]
fn tablesample_targets_and_argument_kinds() {
    let mut db = setup();
    db.apply_sql(
        "CREATE VIEW vt AS SELECT * FROM t;
         CREATE SEQUENCE sq;
         CREATE MATERIALIZED VIEW mv AS SELECT * FROM t;
         CREATE TABLE pt (k int) PARTITION BY RANGE (k);",
    )
    .unwrap();
    for sql in [
        "SELECT * FROM vt TABLESAMPLE SYSTEM (1)",
        "SELECT * FROM sq TABLESAMPLE SYSTEM (1)",
        "WITH c AS (SELECT * FROM t) SELECT * FROM c TABLESAMPLE SYSTEM (1)",
    ] {
        let err = assert_err_prefix(
            &db,
            sql,
            "TABLESAMPLE clause can only be applied to tables and materialized views",
        );
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
    }
    for sql in [
        "SELECT * FROM mv TABLESAMPLE SYSTEM (1)",
        "SELECT * FROM pt TABLESAMPLE BERNOULLI (50)",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let err = assert_err_prefix(
        &db,
        "SELECT * FROM t TABLESAMPLE SYSTEM (sum(1))",
        "aggregate functions are not allowed in functions in FROM",
    );
    assert!(matches!(err, AnalyzeError::GroupingError(_)), "{err:?}");
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

/// findTargetlistEntrySQL92: a constant ORDER BY / GROUP BY / DISTINCT ON
/// item is a position, so it must be an integer (42601) — inside grouping
/// sets and implicit rows too, and for set operations and VALUES.
#[test]
fn non_integer_constant_in_sort_and_group_clauses() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT id FROM t ORDER BY 'x'",
            "non-integer constant in ORDER BY",
        ),
        (
            "SELECT id FROM t ORDER BY 1.5",
            "non-integer constant in ORDER BY",
        ),
        (
            "SELECT id FROM t ORDER BY NULL",
            "non-integer constant in ORDER BY",
        ),
        (
            "SELECT id FROM t ORDER BY true",
            "non-integer constant in ORDER BY",
        ),
        (
            "SELECT id FROM t GROUP BY 'x'",
            "non-integer constant in GROUP BY",
        ),
        (
            "SELECT id FROM t GROUP BY 1.5",
            "non-integer constant in GROUP BY",
        ),
        (
            "SELECT id FROM t GROUP BY ROLLUP('x')",
            "non-integer constant in GROUP BY",
        ),
        (
            "SELECT id FROM t GROUP BY (id, 'x')",
            "non-integer constant in GROUP BY",
        ),
        (
            "SELECT DISTINCT ON ('x') id FROM t",
            "non-integer constant in DISTINCT ON",
        ),
        (
            "SELECT 1 UNION SELECT 2 ORDER BY 'x'",
            "non-integer constant in ORDER BY",
        ),
        (
            "VALUES (1) ORDER BY 'x'",
            "non-integer constant in ORDER BY",
        ),
    ] {
        let err = assert_err_prefix(&db, sql, msg);
        assert!(matches!(err, AnalyzeError::SyntaxError(_)), "{err:?}");
    }
    // An implicit row's members are grouping items of their own.
    let err = assert_err_prefix(
        &db,
        "SELECT id FROM t GROUP BY (1, 2)",
        "GROUP BY position 2 is not in select list",
    );
    assert!(
        matches!(err, AnalyzeError::InvalidColumnReference(_)),
        "{err:?}"
    );
}

/// A bare VALUES list takes ORDER BY / LIMIT over its own columns.
#[test]
fn values_list_order_by_and_limit() {
    let db = setup();
    let err = assert_err_prefix(
        &db,
        "VALUES (1) ORDER BY 2",
        "ORDER BY position 2 is not in select list",
    );
    assert!(
        matches!(err, AnalyzeError::InvalidColumnReference(_)),
        "{err:?}"
    );
    let s = db
        .analyze("VALUES (1), (2) ORDER BY column1 + 1 LIMIT 1")
        .unwrap();
    assert_cols(&s, vec![c("column1", int4())]);
}

/// transformValuesClause: select_common_type over each column's rows.
#[test]
fn values_rows_need_a_common_column_type() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT a0 FROM (VALUES (-1), (42), (false)) v(a0)",
            "VALUES types integer and boolean cannot be matched",
        ),
        (
            "VALUES (1::int), ('2020-01-01'::date)",
            "VALUES types integer and date cannot be matched",
        ),
        (
            "VALUES (ARRAY[1]), (1)",
            "VALUES types integer[] and integer cannot be matched",
        ),
    ] {
        let err = assert_err_prefix(&db, sql, msg);
        assert!(matches!(err, AnalyzeError::DatatypeMismatch(_)), "{err:?}");
    }
    let s = db
        .analyze("VALUES (1, NULL), (1.5, 'x'), (2::bigint, NULL)")
        .unwrap();
    assert_cols(&s, vec![c("column1", numeric()), cn("column2", text())]);
}

/// A bare name matching several output columns with different expressions
/// is ambiguous (42702) — in GROUP BY only when no input column has it.
#[test]
fn ambiguous_output_column_name_in_sort_and_group_clauses() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT id AS x, a AS x FROM t ORDER BY x",
            "ORDER BY \"x\" is ambiguous",
        ),
        (
            "SELECT DISTINCT ON (x) id AS x, a AS x FROM t",
            "DISTINCT ON \"x\" is ambiguous",
        ),
        (
            "SELECT id AS x, a AS x FROM t GROUP BY x",
            "GROUP BY \"x\" is ambiguous",
        ),
        (
            "SELECT 1 x, 2 x UNION SELECT 1, 2 ORDER BY x",
            "ORDER BY \"x\" is ambiguous",
        ),
    ] {
        let err = assert_err_prefix(&db, sql, msg);
        assert!(matches!(err, AnalyzeError::AmbiguousColumn(_)), "{err:?}");
    }
    for sql in [
        "SELECT id AS x, id AS x FROM t ORDER BY x",
        "SELECT id AS x, t.id AS x FROM t ORDER BY x",
        "SELECT *, id FROM t ORDER BY id",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // GROUP BY prefers the input column `a` over the output alias.
    let err = assert_err_prefix(
        &db,
        "SELECT id AS a, a FROM t GROUP BY a",
        "column \"t.id\" must appear in the GROUP BY clause",
    );
    assert!(matches!(err, AnalyzeError::GroupingError(_)), "{err:?}");
}

/// transformLimitClause: WITH TIES needs a row count — a NULL constant is
/// 2201W.
#[test]
fn fetch_with_ties_rejects_null_row_count() {
    let db = setup();
    for sql in [
        "SELECT id FROM t ORDER BY id FETCH FIRST NULL ROW WITH TIES",
        "SELECT * FROM t ORDER BY a FETCH FIRST (NULL) ROWS WITH TIES",
        "SELECT 1 UNION SELECT 2 ORDER BY 1 FETCH FIRST NULL ROWS WITH TIES",
        "VALUES (1) ORDER BY 1 FETCH FIRST NULL ROWS WITH TIES",
    ] {
        let err = assert_err_prefix(
            &db,
            sql,
            "row count cannot be null in FETCH FIRST ... WITH TIES clause",
        );
        assert!(
            matches!(err, AnalyzeError::InvalidRowCountInLimitClause(_)),
            "{err:?}"
        );
    }
    db.analyze("SELECT id FROM t ORDER BY id FETCH FIRST 1 ROW WITH TIES")
        .unwrap();
}

/// get_sort_group_operators: a sort key needs its type's ordering operator
/// and a grouping / DISTINCT / set-operation / PARTITION BY key its
/// equality operator (42883) — json, xml and point have none; GREATEST /
/// LEAST need a comparison function.
#[test]
fn sort_and_group_keys_need_ordering_and_equality_operators() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE tj (id int PRIMARY KEY, js json NOT NULL, p point NOT NULL, x xid);
         CREATE TYPE pj AS (a int, j json);
         CREATE TABLE tc (id int PRIMARY KEY, c pj, arr json[]);",
    )
    .unwrap();
    for (sql, msg) in [
        (
            "SELECT id FROM tj ORDER BY js",
            "could not identify an ordering operator for type json",
        ),
        (
            "SELECT id FROM tj ORDER BY p",
            "could not identify an ordering operator for type point",
        ),
        (
            "SELECT id FROM tj ORDER BY x",
            "could not identify an ordering operator for type xid",
        ),
        (
            "SELECT id FROM tc ORDER BY c",
            "could not identify an ordering operator for type pj",
        ),
        (
            "SELECT id FROM tc ORDER BY arr",
            "could not identify an ordering operator for type json[]",
        ),
        (
            "SELECT DISTINCT js FROM tj",
            "could not identify an equality operator for type json",
        ),
        (
            "SELECT js FROM tj UNION SELECT js FROM tj",
            "could not identify an equality operator for type json",
        ),
        (
            "SELECT js FROM tj GROUP BY js",
            "could not identify an equality operator for type json",
        ),
        (
            "SELECT count(DISTINCT js) FROM tj",
            "could not identify an equality operator for type json",
        ),
        (
            "SELECT id FROM tj GROUP BY id, point '(1,1)'",
            "could not identify an equality operator for type point",
        ),
        (
            "SELECT point '(1,1)' UNION SELECT point '(1,1)'",
            "could not identify an equality operator for type point",
        ),
        (
            "SELECT json_build_object() INTERSECT SELECT json_build_object()",
            "could not identify an equality operator for type json",
        ),
        (
            "SELECT xmlelement(name a) UNION SELECT xmlelement(name b)",
            "could not identify an equality operator for type xml",
        ),
        (
            "SELECT DISTINCT ON (point '(1,1)') id FROM tj",
            "could not identify an equality operator for type point",
        ),
        (
            "SELECT id FROM tj GROUP BY ROLLUP (point '(1,1)')",
            "could not identify an equality operator for type point",
        ),
        (
            "SELECT row_number() OVER (PARTITION BY p) FROM tj",
            "could not identify an equality operator for type point",
        ),
        (
            "SELECT row_number() OVER (ORDER BY p) FROM tj",
            "could not identify an ordering operator for type point",
        ),
        (
            "SELECT array_agg(id ORDER BY p) FROM tj",
            "could not identify an ordering operator for type point",
        ),
        (
            "SELECT p FROM tj UNION ALL SELECT p FROM tj ORDER BY 1",
            "could not identify an ordering operator for type point",
        ),
        (
            "SELECT greatest(p, p) FROM tj",
            "could not identify a comparison function for type point",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::UndefinedFunction(_), msg);
    }
    for sql in [
        "SELECT p FROM tj UNION ALL SELECT p FROM tj",
        "SELECT DISTINCT x FROM tj",
        "SELECT id FROM tj ORDER BY p <-> point '(0,0)'",
        "SELECT DISTINCT ROW(id, p) FROM tj",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
