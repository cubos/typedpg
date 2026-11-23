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
        // A subquery over VALUES (or a function) alone, pulled up into the
        // nullable side, leaves no row mark there.
        "SELECT * FROM t LEFT JOIN (VALUES (1, 'x')) v(x, y) ON v.x = t.id FOR UPDATE",
        "SELECT * FROM t FULL JOIN (SELECT n FROM generate_series(1, 2) g(n)) s ON s.n = t.id \
         FOR UPDATE OF s",
        "SELECT * FROM (VALUES (1)) v(x) LEFT JOIN (VALUES (2)) w(x) ON w.x = v.x FOR UPDATE",
        // The WHERE reduces the join (`NOT (… NOT IN …)` is `IN`).
        "SELECT * FROM t LEFT JOIN u ON u.t_id = t.id WHERE u.t_id IN (1, 2) FOR UPDATE",
        "SELECT * FROM t LEFT JOIN u ON u.t_id = t.id \
         WHERE NOT (u.t_id NOT IN (SELECT id FROM t)) FOR UPDATE",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // The planner rejects locking the nullable side of an outer join (PG
    // raises it when the prepared statement is executed).
    for sql in [
        "SELECT * FROM t LEFT JOIN u ON u.t_id = t.id FOR UPDATE",
        "SELECT * FROM t LEFT JOIN u ON u.t_id = t.id FOR UPDATE OF u",
        // Planned on its own (a LIMIT keeps it from being pulled up), the
        // subquery's own entry carries the mark.
        "SELECT * FROM t LEFT JOIN (SELECT x FROM (VALUES (1)) v(x) LIMIT 1) q ON q.x = t.id \
         FOR UPDATE",
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

/// `transformLockingClause` searches the range table in order for the
/// first entry of the name — one an aliased join hides included — so a
/// name both hidden and visible means whichever entry comes first: a
/// join's sides come before the join, and each FROM item after the
/// previous ones. Entries of an enclosing level are never named.
#[test]
fn locking_clause_takes_the_first_range_table_entry_of_the_name() {
    let mut db = setup();
    db.apply_sql("CREATE TABLE w (id int PRIMARY KEY);")
        .unwrap();
    let nullable = "FOR UPDATE cannot be applied to the nullable side of an outer join";
    for (sql, msg) in [
        // The hidden `u` (the join's nullable side) comes first.
        (
            "SELECT * FROM (t LEFT JOIN u ON u.t_id = t.id) j, u u2, w u FOR UPDATE OF u",
            nullable,
        ),
        // The hidden `g` relation comes before the visible function `g`;
        // the visible function `g` before the hidden one.
        (
            "SELECT * FROM generate_series(1, 2) g, (t g JOIN u ON true) j FOR UPDATE OF g",
            "FOR UPDATE cannot be applied to a function",
        ),
        // In a join's sides, the left one comes first.
        (
            "SELECT * FROM (u t LEFT JOIN (t JOIN w ON true) x ON true) y FOR UPDATE OF t",
            "",
        ),
        (
            "SELECT * FROM (w LEFT JOIN (t JOIN u ON true) x ON true) y, t FOR UPDATE OF t",
            nullable,
        ),
        // An entry an aliased join hides is on the nullable side of the
        // outer joins the hiding join is.
        (
            "SELECT * FROM (w LEFT JOIN (t JOIN u ON true) x ON true) y FOR UPDATE OF t",
            nullable,
        ),
        (
            "SELECT * FROM (w LEFT JOIN (t JOIN u ON true) x ON true) y FOR UPDATE",
            nullable,
        ),
        // A JSON_TABLE is a table function.
        (
            "SELECT * FROM t, JSON_TABLE('[1]'::jsonb, '$[*]' COLUMNS (v int PATH '$')) jt \
             FOR UPDATE OF jt",
            "FOR UPDATE cannot be applied to a table function",
        ),
        // An enclosing level's entries, visible or hidden, are not this
        // level's.
        (
            "SELECT * FROM (t JOIN u ON true) j, LATERAL (SELECT 1 FROM w FOR UPDATE OF t) s",
            "relation \"t\" in FOR UPDATE clause not found in FROM clause",
        ),
        (
            "SELECT * FROM (t JOIN u ON true) j \
             WHERE EXISTS (SELECT 1 FROM w FOR UPDATE OF u)",
            "relation \"u\" in FOR UPDATE clause not found in FROM clause",
        ),
    ] {
        if msg.is_empty() {
            db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        } else {
            assert_err_prefix(&db, sql, msg);
        }
    }
    // The visible `g` relation comes before the hidden function `g`.
    for sql in [
        "SELECT * FROM t g, (generate_series(1, 2) g JOIN u ON true) j FOR UPDATE OF g",
        "SELECT * FROM (t JOIN u ON true) j, (u LEFT JOIN w ON true) k FOR UPDATE OF u",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// transformValuesClause: a bare VALUES has no relation to lock (0A000),
/// while a VALUES in FROM is skipped by an unnamed locking clause.
#[test]
fn for_update_cannot_apply_to_values() {
    let db = setup();
    for (sql, msg) in [
        (
            "VALUES (1) FOR UPDATE",
            "FOR UPDATE cannot be applied to VALUES",
        ),
        (
            "VALUES (1), (2) FOR SHARE",
            "FOR SHARE cannot be applied to VALUES",
        ),
        (
            "VALUES (1) ORDER BY 1 FOR KEY SHARE",
            "FOR KEY SHARE cannot be applied to VALUES",
        ),
        (
            "SELECT * FROM (VALUES (1) FOR NO KEY UPDATE) v",
            "FOR NO KEY UPDATE cannot be applied to VALUES",
        ),
    ] {
        let err = assert_err_prefix(&db, sql, msg);
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
    }
    db.analyze("SELECT * FROM (VALUES (1)) v FOR UPDATE")
        .unwrap();
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
    // A qual above the outer join may reduce it to an inner one
    // (`reduce_outer_joins`): the view's or subquery's own WHERE, or once
    // pulled up, a qual of the level locking it.
    db.apply_sql(
        "CREATE VIEW vw AS SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id = t.id WHERE u.x <> '';",
    )
    .unwrap();
    for sql in [
        "SELECT * FROM vw FOR UPDATE",
        "SELECT * FROM (SELECT t.id FROM t LEFT JOIN u ON u.t_id = t.id WHERE u.x = 'a') q \
         FOR UPDATE",
        "SELECT * FROM t t2 JOIN (SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id = t.id) q \
         ON q.x = t2.b FOR UPDATE",
        "SELECT * FROM (SELECT u.x FROM t RIGHT JOIN u ON u.t_id = t.id WHERE t.b || 'x' <= 'a' \
         LIMIT 3) q FOR UPDATE",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // The join's own ON reduces nothing.
    let err = assert_err_prefix(
        &db,
        "SELECT * FROM (SELECT t.id FROM t LEFT JOIN u ON u.t_id = t.id AND u.x = 'a') q \
         FOR UPDATE",
        "FOR UPDATE cannot be applied to the nullable side of an outer join",
    );
    assert!(
        matches!(err, AnalyzeError::FeatureNotSupported(_)),
        "{err:?}"
    );
    for sql in [
        "SELECT * FROM vp FOR UPDATE",
        "SELECT * FROM va, t FOR UPDATE OF t",
        "SELECT * FROM (SELECT * FROM vp) q FOR UPDATE",
        "SELECT * FROM t LEFT JOIN (SELECT * FROM u) q ON q.t_id = t.id FOR UPDATE OF t",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// `make_outerjoininfo` refuses a locked relation on the nullable side of
/// an outer join `reduce_outer_joins` leaves outer — and that reduction
/// reads only the strictness of the quals handed down to the join
/// (`find_nonnullable_rels`), not what else proves the side non-NULL.
#[test]
fn only_strict_quals_reduce_a_locked_outer_join() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE w (id int PRIMARY KEY, t_id int NOT NULL REFERENCES t, x text NOT NULL);
         CREATE VIEW vl AS SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id < t.id;
         CREATE VIEW vn AS SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id < t.id
             WHERE u IS NOT NULL;
         CREATE VIEW vc AS SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id < t.id
             WHERE coalesce(u.x, '') = 'a';
         CREATE VIEW vu AS SELECT t.id FROM t LEFT JOIN u USING (id) WHERE id = 1;
         CREATE VIEW vx AS SELECT t.id FROM t LEFT JOIN (u CROSS JOIN t t3) ON t3.id = t.id
             WHERE t3.a = 1;",
    )
    .unwrap();
    let nullable = "FOR UPDATE cannot be applied to the nullable side of an outer join";
    for sql in [
        // Not strict: a whole-row test, a COALESCE (even a NULL for it
        // that makes the qual FALSE), an ordering row comparison, IS
        // DISTINCT FROM.
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id WHERE u IS NOT NULL FOR UPDATE OF u",
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id WHERE coalesce(u.x, 'z') = 'a' \
         FOR UPDATE OF u",
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id WHERE t.a = coalesce(u.id, 0) \
         FOR UPDATE OF u",
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id WHERE (u.id, u.t_id) < (1, 2) \
         FOR UPDATE OF u",
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id WHERE u.x IS DISTINCT FROM 'a' \
         FOR UPDATE OF u",
        // A foreign key: every child has its parent, but no qual says so.
        "SELECT 1 FROM w LEFT JOIN t ON w.t_id = t.id FOR UPDATE OF t",
        // A strict set-returning function's argument, a LATERAL subquery's
        // WHERE: no qual above the join.
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id \
         CROSS JOIN LATERAL generate_series(1, u.id) g FOR UPDATE OF u",
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id \
         CROSS JOIN LATERAL (SELECT 1 FROM w WHERE w.id = u.id) s FOR UPDATE OF u",
        // Through views and subqueries: their own quals, the level's.
        "SELECT * FROM vn FOR UPDATE",
        "SELECT * FROM vc FOR UPDATE",
        "SELECT * FROM vu FOR UPDATE",
        "SELECT * FROM vl WHERE vl.x IS NULL FOR UPDATE",
        "SELECT * FROM vl WHERE coalesce(vl.x, '') = 'a' FOR UPDATE",
        "SELECT * FROM (SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id < t.id) s \
         WHERE s.id = 1 FOR UPDATE",
        // A pulled-up COALESCE on the nullable side is a PlaceHolderVar
        // over two relations: strict in neither.
        "SELECT * FROM t t2 LEFT JOIN (SELECT t.id, coalesce(u.x, '') AS c \
         FROM t LEFT JOIN u ON u.t_id < t.id) s ON true WHERE s.c = 'a' FOR UPDATE OF s",
        // The relations an aliased join hides are in the range table.
        "SELECT 1 FROM (t LEFT JOIN u ON u.t_id < t.id) j FOR UPDATE",
        "SELECT 1 FROM (t LEFT JOIN u ON u.t_id < t.id) j FOR UPDATE OF u",
    ] {
        let err = assert_err_prefix(&db, sql, nullable);
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
    }
    for sql in [
        // Strict: BETWEEN's bounds (an AND of comparisons), a NOT over IS
        // NULL (IS NOT NULL), a row's fields' tests, an EXISTS pulled up into
        // a semi-join, a join below forced by a strict qual on another of
        // its relations.
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id WHERE t.id BETWEEN 1 AND u.id \
         FOR UPDATE OF u",
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id WHERE NOT (u.x IS NULL) FOR UPDATE OF u",
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id WHERE ROW(u.x, u.id) IS NOT NULL \
         FOR UPDATE OF u",
        "SELECT 1 FROM t LEFT JOIN u ON u.t_id < t.id \
         WHERE EXISTS (SELECT 1 FROM w WHERE w.id = u.id) FOR UPDATE OF u",
        "SELECT * FROM vx FOR UPDATE",
        // A pulled-up view's whole row is a row constructor of its columns.
        "SELECT * FROM vl WHERE vl IS NOT NULL FOR UPDATE",
        "SELECT * FROM (SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id < t.id) s(i, y) \
         WHERE s.y = 'a' FOR UPDATE",
        "SELECT 1 FROM (t LEFT JOIN u ON u.t_id < t.id) j WHERE j.x = 'a' FOR UPDATE OF u",
        "SELECT 1 FROM (t LEFT JOIN u ON u.t_id < t.id) j FOR UPDATE OF t",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// Two locking errors at once: PG raises the parser's (`transformLockingClause`
/// over every clause: a missing entry, a subquery's blocker) before the
/// planner's, and among those `make_outerjoininfo` over the level's joins
/// before the views it plans on their own (`CheckSelectLocking` in
/// `preprocess_rowmarks`).
#[test]
fn locking_errors_come_parser_first_then_planner() {
    let mut db = setup();
    db.apply_sql(
        "CREATE VIEW va AS SELECT count(*) AS c FROM t;
         CREATE VIEW vl AS SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id < t.id;",
    )
    .unwrap();
    for (sql, msg) in [
        (
            "SELECT * FROM t LEFT JOIN u ON u.t_id = t.id FOR UPDATE OF u FOR SHARE OF missing",
            "relation \"missing\" in FOR SHARE clause not found in FROM clause",
        ),
        (
            "SELECT * FROM t LEFT JOIN u ON u.t_id = t.id, (SELECT DISTINCT a FROM t) s \
             FOR UPDATE OF u FOR SHARE OF s",
            "FOR SHARE is not allowed with DISTINCT clause",
        ),
        (
            "SELECT * FROM va, t LEFT JOIN u ON u.t_id = t.id FOR UPDATE OF va FOR SHARE OF u",
            "FOR SHARE cannot be applied to the nullable side of an outer join",
        ),
        (
            "SELECT * FROM (SELECT * FROM va) q, t LEFT JOIN u ON u.t_id = t.id \
             FOR UPDATE OF q FOR SHARE OF u",
            "FOR SHARE cannot be applied to the nullable side of an outer join",
        ),
        (
            "SELECT * FROM vl, va FOR UPDATE OF va FOR SHARE OF vl",
            "FOR SHARE cannot be applied to the nullable side of an outer join",
        ),
    ] {
        assert_err_prefix(&db, sql, msg);
    }
}

/// The rewriter marks only a view's relations and subqueries for locking
/// (`markQueryForLocking`), and the planner checks the view's query
/// (`CheckSelectLocking` in `preprocess_rowmarks`) only when it has a row
/// mark: a view reading functions or CTEs alone, or a set operation (whose
/// arms are no part of its jointree), is locked through without error.
#[test]
fn a_view_without_row_marks_is_not_checked() {
    let mut db = setup();
    db.apply_sql(
        "CREATE VIEW vu AS SELECT id FROM t UNION SELECT id FROM u;
         CREATE VIEW vf AS SELECT bool_or(v) AS c FROM unnest(ARRAY[true]) g(v) GROUP BY v;
         CREATE VIEW vc AS WITH c AS (SELECT 1 AS v) SELECT count(*) FROM c;
         CREATE VIEW vs AS SELECT bool_or(v) AS c FROM (SELECT * FROM unnest(ARRAY[true]) g(v)) s
             GROUP BY v;",
    )
    .unwrap();
    for sql in [
        "SELECT * FROM vu FOR UPDATE",
        "SELECT * FROM vf FOR UPDATE",
        "SELECT * FROM vc FOR UPDATE",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_err_prefix(
        &db,
        "SELECT * FROM vs FOR UPDATE",
        "FOR UPDATE is not allowed with GROUP BY clause",
    );
}

/// `reduce_outer_joins` reads the quals `eval_const_expressions` folded: a
/// CASE with a constant WHEN is its branch, a COALESCE its one argument
/// left past the NULL constants, an AND / OR loses its TRUE / FALSE
/// arguments — so a qual strict in the nullable side only once folded
/// reduces the join, and its rows can be locked.
#[test]
fn locking_the_nullable_side_sees_the_folded_quals() {
    let db = setup();
    let join = "SELECT t.id FROM t LEFT JOIN u ON u.t_id = t.id";
    for qual in [
        "CASE WHEN true THEN u.x END = 'a'",
        "CASE WHEN 1 = 1 THEN u.x ELSE 'b' END = 'a'",
        "CASE WHEN false THEN 'b' ELSE u.x END = 'a'",
        "CASE WHEN NULL THEN 'b' WHEN true THEN u.x END = 'a'",
        "COALESCE(NULL, u.x) = 'a'",
        "u.x = 'a' OR false",
        "u.x = 'a' OR NULL",
        "NOT (u.x = 'a' AND true)",
        // `x IS DISTINCT FROM NULL` is the NullTest `x IS NOT NULL`, the
        // NULL once folded too.
        "u.x IS DISTINCT FROM NULL::text",
        "u.x IS DISTINCT FROM CASE t.a WHEN NULL THEN 'b' END",
        // A custom plan's parameters are constants: a NULL `$1` folds
        // these to `u.x = 'a'`.
        "COALESCE($1, u.x) = 'a'",
        "u.x = 'a' OR $1",
    ] {
        let sql = format!("{join} WHERE {qual} FOR UPDATE");
        db.analyze(&sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for qual in [
        "CASE WHEN t.a > 0 THEN u.x END = 'a'",
        "u.x = 'a' OR true",
        "u.x = 'a' OR t.a = 1",
    ] {
        let sql = format!("{join} WHERE {qual} FOR UPDATE");
        assert_err_prefix(
            &db,
            &sql,
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
        );
    }
}

/// A view or subquery the planner doesn't pull up (a LIMIT, its own
/// locking clause) is planned on its own — unless the planner proves it
/// empty first (`relation_excluded_by_constraints`: a constant FALSE
/// qual, or a custom plan's qual over a NULL parameter): then its
/// nullable side is never refused. A view's query is only planned where a
/// statement reads it: CREATE VIEW stores one that locks a nullable side.
#[test]
fn a_view_planned_alone_and_proven_empty_is_never_planned() {
    let mut db = setup();
    db.apply_sql(
        "CREATE VIEW vl AS SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id = t.id;
         CREATE VIEW vlim AS SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id = t.id LIMIT 10;
         CREATE VIEW vlock AS SELECT t.id, u.x FROM t LEFT JOIN u ON u.t_id = t.id FOR UPDATE OF u;",
    )
    .unwrap();
    for sql in [
        "SELECT * FROM vlim WHERE false FOR UPDATE",
        "SELECT * FROM vlim WHERE $1 FOR UPDATE",
        "SELECT * FROM (SELECT t.id FROM t LEFT JOIN u ON u.t_id = t.id LIMIT 3) q \
         WHERE false FOR UPDATE",
        "SELECT * FROM vlock WHERE false",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for sql in [
        // Pulled up, the view's join is this level's: planned whatever the
        // WHERE folds to.
        "SELECT * FROM vl WHERE false FOR UPDATE",
        "SELECT * FROM vlim FOR UPDATE",
        "SELECT * FROM vlim, t WHERE false FOR UPDATE OF vlim",
        "SELECT * FROM vlock",
    ] {
        assert_err_prefix(
            &db,
            sql,
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
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
        // An item sorting a target sorted already is dropped (in either
        // direction), however it names it.
        "SELECT DISTINCT ON (a, b) a, b FROM t ORDER BY a, b, id, 1",
        "SELECT DISTINCT ON (a, b) a, b FROM t ORDER BY a, b, id, 2 DESC",
        "SELECT DISTINCT ON (a) a, b FROM t ORDER BY a, b, a DESC NULLS FIRST",
        "SELECT DISTINCT ON (t.a) a AS x, b FROM t ORDER BY t.a, b, x",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// A cast leaving its operand as it is (the same type and typmod) is gone
/// once PG transforms the expression, so it doesn't keep an ORDER BY
/// item from matching the select list or the DISTINCT ON list, nor the
/// argument list of an aggregate with DISTINCT.
#[test]
fn noop_casts_match_the_expression_they_cast() {
    let db = setup_wide();
    for sql in [
        "SELECT DISTINCT a FROM t ORDER BY a::int",
        "SELECT DISTINCT a::int AS x FROM t ORDER BY a",
        "SELECT DISTINCT a + 1 AS x FROM t ORDER BY a::int + 1",
        "SELECT DISTINCT c FROM t ORDER BY c::varchar(10)",
        "SELECT DISTINCT ON (a::int) a, b FROM t ORDER BY a, b",
        "SELECT DISTINCT ON (a) a, b FROM t ORDER BY a::int, b",
        "SELECT DISTINCT ON ((a + 1)::int) a + 1 AS k, b FROM t ORDER BY a + 1, b",
        "SELECT array_agg(DISTINCT a ORDER BY a::int) FROM t",
        "SELECT array_agg(DISTINCT (a + 1)::int ORDER BY a + 1) FROM t",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for sql in [
        "SELECT DISTINCT a FROM t ORDER BY a::int8",
        "SELECT DISTINCT c FROM t ORDER BY c::varchar",
    ] {
        let err = assert_err_prefix(
            &db,
            sql,
            "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
        );
        assert!(
            matches!(err, AnalyzeError::InvalidColumnReference(_)),
            "{sql}: {err:?}"
        );
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

/// transformSelectStmt: INTO only belongs to the top-level SELECT — not a
/// subquery, a CTE, a set-operation member or the SELECT under INSERT
/// (42601).
#[test]
fn select_into_is_only_allowed_at_the_top() {
    let db = setup_wide();
    for sql in [
        "SELECT * FROM (SELECT 1 INTO f) bar",
        "INSERT INTO t (id, a) SELECT 1, 2 INTO f",
        "SELECT (SELECT 1 INTO f)",
        "SELECT 1 WHERE EXISTS (SELECT 1 INTO f)",
        "WITH w AS (SELECT 1 INTO f) SELECT * FROM w",
        "SELECT * FROM (SELECT 1 INTO f UNION SELECT 2) x",
        // A member with its own ORDER BY is a subquery of the operation.
        "SELECT 1 UNION (SELECT 2 INTO f UNION SELECT 3 ORDER BY 1)",
    ] {
        let err = assert_err_prefix(&db, sql, "SELECT ... INTO is not allowed here");
        assert!(
            matches!(err, AnalyzeError::SyntaxError(_)),
            "{sql}: {err:?}"
        );
    }
    // transformSetOperationTree: a later member's INTO has its own wording.
    for sql in [
        "SELECT 1 UNION (SELECT 2 INTO f)",
        "SELECT 1 UNION SELECT 2 UNION (SELECT 3 INTO f)",
        "SELECT 1 UNION (SELECT 2 INTO f UNION SELECT 3)",
    ] {
        let err = assert_err_prefix(
            &db,
            sql,
            "INTO is only allowed on first SELECT of UNION/INTERSECT/EXCEPT",
        );
        assert!(
            matches!(err, AnalyzeError::SyntaxError(_)),
            "{sql}: {err:?}"
        );
    }
    // The first member of a set operation carries the statement's INTO.
    let s = db.analyze("SELECT 1 INTO f UNION SELECT 2").unwrap();
    assert_cols(&s, vec![]);
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

/// transformValuesClause resolves and coerces one column after the other:
/// an earlier column's failing cell is reported before a later column's
/// type mismatch or failing cell, whatever the row.
#[test]
fn values_columns_fail_in_column_order() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT * FROM (VALUES ('x', -1), (0.0, now())) AS v(a0, a1)",
            "invalid input syntax for type numeric: \"x\"",
        ),
        (
            "SELECT * FROM (VALUES (1, 'a'), ('b', 2.5)) AS v(a0, a1)",
            "invalid input syntax for type integer: \"b\"",
        ),
        (
            "VALUES (1, 'a', true), (2, 2.5, 'c')",
            "invalid input syntax for type numeric: \"a\"",
        ),
    ] {
        assert_err_prefix(&db, sql, msg);
    }
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

/// findTargetlistEntrySQL92: a bare ORDER BY / DISTINCT ON name is looked
/// up among the output columns *first* — whatever their origin (a plain
/// column reference, a `t.*` expansion), not only `AS` aliases — and only
/// falls back to the FROM scope when no output column has it. So a name
/// two FROM items share is fine when the select list projects it once,
/// and is `ORDER BY "id" is ambiguous` (not `column reference … is
/// ambiguous`) when it projects it twice from different columns.
#[test]
fn sort_keys_match_unaliased_output_columns_before_from_scope() {
    let db = setup();
    for sql in [
        "SELECT u.id FROM t, u ORDER BY id",
        "SELECT t.* FROM t, u ORDER BY id",
        "SELECT DISTINCT ON (id) u.id FROM t, u",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for sql in [
        "SELECT t.id, u.id FROM t, u ORDER BY id",
        "SELECT * FROM t, u ORDER BY id",
    ] {
        let err = assert_err_prefix(&db, sql, "ORDER BY \"id\" is ambiguous");
        assert!(matches!(err, AnalyzeError::AmbiguousColumn(_)), "{err:?}");
    }
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

/// An output column a `(row).*` contributes is named after its field, and
/// one a `t.*` contributes is in the select list for SELECT DISTINCT's
/// ORDER BY (findTargetlistEntrySQL99 compares it with the expanded
/// entries); PG 19.0 runs each query.
#[test]
fn sort_keys_match_columns_stars_contribute() {
    let mut db = setup();
    db.apply_sql("CREATE TYPE pair AS (f1 int, f2 int);")
        .unwrap();
    for sql in [
        "SELECT (r).* FROM (SELECT ROW(1, 2)::pair AS r) q ORDER BY f1",
        "SELECT DISTINCT ON (f2) (r).* FROM (SELECT ROW(1, 2)::pair AS r) q",
        "SELECT DISTINCT t.* FROM t ORDER BY t.id",
        "SELECT DISTINCT t.* FROM t, u ORDER BY id",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
