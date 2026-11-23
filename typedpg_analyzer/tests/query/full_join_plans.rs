//! The planner's FULL JOIN check (`populate_joinrel_with_paths`): a FULL
//! JOIN is a merge join or a hash join, so one whose join clauses neither
//! can use fails every execution with `FULL JOIN is only supported with
//! merge-joinable or hash-joinable join conditions` (0A000) — unless
//! `reduce_outer_joins` reduced it, or the planner proved both its sides
//! (or the join, under a constant FALSE qual) empty first.

use crate::common::*;

const MSG: &str =
    "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions";

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE a (id int PRIMARY KEY, x int, s text, p point, r int4range, arr int[],
                         ps point[]);
         CREATE TABLE b (id int PRIMARY KEY, y int, s text, p point, r int4range, arr int[],
                         ps point[]);
         CREATE FUNCTION sql_eq(int, int) RETURNS bool LANGUAGE sql IMMUTABLE
             AS 'SELECT $1 = $2';
         CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = sql_eq);
         CREATE FUNCTION pl_eq(int, int) RETURNS bool LANGUAGE plpgsql IMMUTABLE
             AS 'BEGIN RETURN $1 = $2; END';
         CREATE OPERATOR =#= (LEFTARG = int, RIGHTARG = int, FUNCTION = pl_eq);",
    )
    .unwrap();
    db
}

#[track_caller]
fn assert_unplannable(db: &PgCatalog, sql: &str) {
    match db.analyze(sql) {
        Err(e @ AnalyzeError::FeatureNotSupported(_)) if e.to_string().starts_with(MSG) => {}
        other => panic!("{sql}: expected `{MSG}`, got {other:?}"),
    }
}

#[test]
fn a_full_join_no_merge_or_hash_join_can_use_fails() {
    let db = setup();
    for sql in [
        // Not merge- nor hash-joinable operators (`oprcanmerge`,
        // `oprcanhash`): `<`, point's `~=`, a user operator without
        // MERGES / HASHES.
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y",
        "SELECT 1 FROM a FULL JOIN b ON a.p ~= b.p",
        "SELECT 1 FROM a FULL JOIN b ON a.x =#= b.y",
        // A clause that reads one side only, or the same side on both of
        // its operands, is no join clause (`can_join`).
        "SELECT 1 FROM a FULL JOIN b ON b.y = 1",
        "SELECT 1 FROM a FULL JOIN b ON a.x = b.y + a.x",
        // An array's `=` hashes and merges only if its element does
        // (`hash_array`, `btarraycmp`): point has neither.
        "SELECT 1 FROM a FULL JOIN b ON a.ps = b.ps",
        // Not an `OpExpr`: OR, ScalarArrayOpExpr, BooleanTest, a CASE whose
        // WHEN stays, a DistinctExpr over a nullable column.
        "SELECT 1 FROM a FULL JOIN b ON a.x = b.y OR a.s = b.s",
        "SELECT 1 FROM a FULL JOIN b ON NOT (a.x = b.y OR a.s = b.s)",
        "SELECT 1 FROM a FULL JOIN b ON a.x = ANY(ARRAY[b.y])",
        "SELECT 1 FROM a FULL JOIN b ON (a.x = b.y) IS TRUE",
        "SELECT 1 FROM a FULL JOIN b ON CASE WHEN a.x > 0 THEN a.x = b.y END",
        "SELECT 1 FROM a FULL JOIN b ON a.x IS NOT DISTINCT FROM b.y",
        // Below another join, in a subquery, a view, UPDATE and DELETE.
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y LEFT JOIN a a2 ON a2.id = b.id",
        "SELECT (SELECT 1 FROM a FULL JOIN b ON a.x < b.y LIMIT 1)",
        "SELECT 1 FROM (a FULL JOIN b ON a.x < b.y) JOIN a a3 ON true WHERE false",
        "UPDATE a SET x = 1 FROM b FULL JOIN a a2 ON b.y < a2.x WHERE a.id = 1",
        "DELETE FROM a USING b FULL JOIN a a2 ON b.y < a2.x",
        // A FROM subquery no qual around reads.
        "SELECT * FROM (SELECT a.id, b.y FROM a FULL JOIN b ON a.x < b.y) s",
        // Only one side proven empty.
        "SELECT 1 FROM (SELECT * FROM a WHERE false) a FULL JOIN b ON a.x < b.y",
    ] {
        assert_unplannable(&db, sql);
    }
}

#[test]
fn a_full_join_a_merge_or_hash_join_can_use_is_planned() {
    let db = setup();
    for sql in [
        "SELECT 1 FROM a FULL JOIN b ON a.x = b.y",
        "SELECT 1 FROM a FULL JOIN b ON a.x + 1 = b.y",
        "SELECT 1 FROM a FULL JOIN b ON a.x::text = b.y::text",
        "SELECT 1 FROM a FULL JOIN b ON (a.x = 1) = (b.y = 1)",
        // A hash join evaluates the other clauses as a join filter.
        "SELECT 1 FROM a FULL JOIN b ON a.x = b.y AND a.s < b.s",
        // `negate_clause`: `NOT (a <> b)` is `a = b`.
        "SELECT 1 FROM a FULL JOIN b ON NOT (a.x <> b.y)",
        // `find_duplicate_ors`: `x OR x` is `x`.
        "SELECT 1 FROM a FULL JOIN b ON a.x = b.y OR a.x = b.y",
        // `simplify_or_arguments` drops a FALSE arm.
        "SELECT 1 FROM a FULL JOIN b ON a.x = b.y OR false",
        // A SQL function's operator is inlined (`a.x = b.y`).
        "SELECT 1 FROM a FULL JOIN b ON a.x === b.y",
        // The range, array and row equalities over hashable elements.
        "SELECT 1 FROM a FULL JOIN b ON a.r = b.r",
        "SELECT 1 FROM a FULL JOIN b ON a.arr = b.arr",
        "SELECT 1 FROM a FULL JOIN b ON (a.x, a.s) = (b.y, b.s)",
        "SELECT 1 FROM a FULL JOIN b USING (r)",
        "SELECT 1 FROM a FULL JOIN b USING (id)",
        // PostgreSQL 19 turns a DistinctExpr over NOT NULL columns into `<>`.
        "SELECT 1 FROM a FULL JOIN b ON a.id IS NOT DISTINCT FROM b.id",
        // A CASE folding to its branch, a constant qual.
        "SELECT 1 FROM a FULL JOIN b ON CASE WHEN true THEN a.x = b.y END",
        "SELECT 1 FROM a FULL JOIN b ON true",
        "SELECT 1 FROM a FULL JOIN b ON false",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y AND false",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn a_reduced_or_empty_full_join_is_never_built() {
    let db = setup();
    for sql in [
        // `reduce_outer_joins`: a strict qual above makes it a LEFT (or
        // inner) join, which a nested loop implements.
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE a.x = 1",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE a.x = 1 AND b.y = 2",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y JOIN a a2 ON a2.id = a.id",
        // A qual of the query a FROM subquery is pulled up into, the ON of
        // the join a MERGE's source is on the nullable side of.
        "SELECT * FROM (SELECT a.id, b.y FROM a FULL JOIN b ON a.x < b.y) s WHERE s.id = 1",
        "MERGE INTO a USING (SELECT a.id, b.y FROM a FULL JOIN b ON a.x < b.y) s \
         ON a.id = s.id WHEN NOT MATCHED BY SOURCE THEN DELETE",
        // PostgreSQL 19 folds `a.id IS NULL` over the primary key to FALSE.
        "SELECT 1 FROM a FULL JOIN b ON CASE WHEN a.id IS NULL THEN false ELSE a.x = b.y END",
        // A constant FALSE qual over the join (`restriction_is_constant_false`),
        // both sides proven empty (`is_dummy_rel`).
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE false",
        "SELECT 1 FROM (SELECT * FROM a WHERE false) a \
         FULL JOIN (SELECT * FROM b WHERE false) b ON a.x < b.y",
        // A custom plan folds a strict clause over a NULL parameter to the
        // NULL constant: FALSE for the whole qual.
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y + $1",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y AND b.id = $1",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE a.id = $1",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// `reduce_outer_joins` reads only the strictness of the quals above the
/// join (`find_nonnullable_rels`): what else proves a side non-NULL — a
/// whole-row test, a COALESCE or CASE evaluated for a NULL column, a CHECK
/// constraint, a LATERAL subquery's WHERE — reduces nothing, and the join
/// stays a FULL one the planner can't build.
#[test]
fn only_strict_quals_reduce_a_full_join() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE c (id int PRIMARY KEY, y int, z int CHECK (y IS NOT NULL OR z IS NOT NULL));",
    )
    .unwrap();
    for sql in [
        // A whole-row IS NOT NULL tests a row (`argisrow`).
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE b IS NOT NULL",
        // A DistinctExpr, a COALESCE, a CASE: not strict.
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE b.y IS DISTINCT FROM 1",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE coalesce(b.y, a.x) = 1",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y \
         WHERE CASE WHEN b.y IS NULL THEN false ELSE true END",
        // A CHECK constraint makes one of the two non-NULL: no qual says so.
        "SELECT 1 FROM a FULL JOIN c ON a.x < c.y WHERE c.y IS NULL AND c.z IS NULL",
        // A LATERAL subquery's WHERE stays in its own FROM item.
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y CROSS JOIN LATERAL \
         (SELECT 1 FROM a a2 WHERE a2.id = b.y) s",
        // A qual of the level around that isn't strict in the subquery's
        // column.
        "SELECT * FROM (SELECT a.id, b.y FROM a FULL JOIN b ON a.x < b.y) s WHERE s.y IS NULL",
        "SELECT * FROM (SELECT a.id, b.y FROM a FULL JOIN b ON a.x < b.y) s \
         WHERE coalesce(s.y, 0) = 0",
    ] {
        assert_unplannable(&db, sql);
    }
    for sql in [
        // Strict once folded, through a NOT, an IN list, a row equality.
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE coalesce(NULL, b.y) = 1 AND a.x = 1",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE NOT (b.y IS NULL OR a.x IS NULL)",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE (a.x, b.y) = (1, 2)",
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y WHERE b.y IN (1, 2) AND a.x IN (1, 2)",
        // An EXISTS pulled up into a semi-join above the join.
        "SELECT 1 FROM a FULL JOIN b ON a.x < b.y \
         WHERE EXISTS (SELECT 1 FROM a a2 WHERE a2.x = a.x AND a2.id = b.y)",
        // A HAVING conjunct without aggregates moves into WHERE.
        "SELECT a.x FROM a FULL JOIN b ON a.x < b.y GROUP BY a.x, b.y HAVING b.y > 0 AND a.x > 0",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// The planner builds the join's paths after parse analysis, the rewriter
/// and constant folding (`preprocess_expression`), and before the
/// statement runs: an UPDATE's or DELETE's errors from those come first, a
/// NULL written into a NOT NULL column (its execution's error) after.
#[test]
fn the_full_join_error_comes_after_folding_before_execution() {
    let mut db = setup();
    db.apply_sql("CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL);")
        .unwrap();
    for (sql, msg) in [
        (
            "UPDATE t SET a = 1 / 0 FROM a FULL JOIN b ON a.x < b.y",
            "division by zero",
        ),
        (
            "UPDATE t SET nosuch = 1 FROM a FULL JOIN b ON a.x < b.y",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "DELETE FROM t USING a FULL JOIN b ON a.x < b.y RETURNING 1 / 0",
            "division by zero",
        ),
        ("UPDATE t SET a = NULL FROM a FULL JOIN b ON a.x < b.y", MSG),
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
}

#[test]
fn a_view_raises_it_where_it_is_planned() {
    let mut db = setup();
    // Stored, the view is never planned.
    db.apply_sql("CREATE VIEW vf AS SELECT a.id FROM a FULL JOIN b ON a.x < b.y;")
        .unwrap();
    assert_unplannable(&db, "SELECT * FROM vf");
}

#[test]
fn merge_with_both_not_matched_actions_is_a_full_join() {
    // transform_MERGE_to_join: NOT MATCHED [BY TARGET] and NOT MATCHED BY
    // SOURCE actions FULL JOIN the target and the source on the ON
    // condition.
    let db = setup();
    assert_unplannable(
        &db,
        "MERGE INTO a USING b ON a.x < b.y WHEN NOT MATCHED THEN INSERT (id) VALUES (b.id) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE",
    );
    for sql in [
        "MERGE INTO a USING b ON a.x = b.y WHEN NOT MATCHED THEN INSERT (id) VALUES (b.id) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE",
        "MERGE INTO a USING b ON a.x < b.y WHEN NOT MATCHED THEN INSERT (id) VALUES (b.id)",
        "MERGE INTO a USING b ON a.x < b.y WHEN NOT MATCHED BY SOURCE THEN DELETE",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn hashes_and_merges_make_an_operator_joinable() {
    // `op_hashjoinable` reads `oprcanhash`, `op_mergejoinable` `oprcanmerge`
    // — and a merge join needs the operator to be a btree equality too
    // (`get_mergejoin_opfamilies`).
    let mut db = setup();
    db.apply_sql("CREATE OPERATOR =%= (LEFTARG = int, RIGHTARG = int, FUNCTION = pl_eq, MERGES);")
        .unwrap();
    assert_unplannable(&db, "SELECT 1 FROM a FULL JOIN b ON a.x =%= b.y");
    db.analyze("SELECT 1 FROM a FULL JOIN b ON a.x = b.y AND a.x =%= b.y")
        .unwrap();
    db.apply_sql(
        "ALTER OPERATOR =#= (int, int) SET (HASHES);
         CREATE OPERATOR CLASS int_hash_alt FOR TYPE int USING hash AS
             OPERATOR 1 =#=, FUNCTION 1 hashint4(int);",
    )
    .unwrap();
    db.analyze("SELECT 1 FROM a FULL JOIN b ON a.x =#= b.y")
        .unwrap();
}
