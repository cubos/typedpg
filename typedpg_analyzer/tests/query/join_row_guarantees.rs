//! Nullability from what joins and subqueries guarantee about their rows:
//! an outer join `ON true` to a side that always yields a row, a scalar
//! subquery that always returns one (an aggregate without GROUP BY, a
//! FROM-less query, a lookup along a foreign key, the outer row finding
//! itself), foreign keys followed through subqueries, CTEs, views and join
//! trees, a FULL join's row having one side or the other, and quals on a
//! subquery's columns — or a LATERAL subquery's WHERE, a strict
//! set-returning function's arguments — proving the row they came from is
//! there. Every NOT NULL below is checked against PostgreSQL 18 by the
//! pg_sanity soundness oracle, over rows that satisfy the constraints.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE a (id int PRIMARY KEY, v int NOT NULL, w int, data jsonb);
         CREATE TABLE b (
             id int PRIMARY KEY, a_id int NOT NULL REFERENCES a, v int NOT NULL,
             w int REFERENCES a
         );
         CREATE TABLE c (id int PRIMARY KEY, b_id int NOT NULL REFERENCES b, v int NOT NULL);
         CREATE TABLE node (id int PRIMARY KEY, parent_id int REFERENCES node, name text NOT NULL);
         CREATE TABLE secret (id int PRIMARY KEY, v int NOT NULL);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE TABLE uses_secret (id int PRIMARY KEY, secret_id int NOT NULL REFERENCES secret);
         CREATE TABLE lax (
             id int PRIMARY KEY, a_id int NOT NULL,
             FOREIGN KEY (a_id) REFERENCES a NOT ENFORCED
         );
         CREATE TABLE inh_parent (id int PRIMARY KEY, v int NOT NULL);
         CREATE TABLE inh_child () INHERITS (inh_parent);
         CREATE VIEW va AS SELECT * FROM a;
         CREATE VIEW vb AS SELECT * FROM b;
         CREATE VIEW va_filtered AS SELECT * FROM a WHERE w > 0;
         CREATE VIEW vbw AS SELECT b.id, a.id AS aid, a.v FROM b LEFT JOIN a ON a.id = b.w;",
    )
    .unwrap();
    db
}

/// The nullability of a query's only column (`true` = nullable).
#[track_caller]
fn nullable(db: &PgCatalog, sql: &str) -> bool {
    let s = db.analyze(sql).unwrap();
    assert_eq!(s.columns.len(), 1, "`{sql}` should have one column");
    s.columns[0].nullable
}

#[track_caller]
fn assert_not_null(db: &PgCatalog, sqls: &[&str]) {
    for sql in sqls {
        assert!(!nullable(db, sql), "`{sql}` should be NOT NULL");
    }
}

#[track_caller]
fn assert_nullable(db: &PgCatalog, sqls: &[&str]) {
    for sql in sqls {
        assert!(nullable(db, sql), "`{sql}` should stay nullable");
    }
}

// ── Outer joins ON true to a side that always yields a row ──────────────────

#[test]
fn an_outer_join_on_true_to_a_one_row_side_never_null_extends_it() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT s.n FROM b LEFT JOIN LATERAL (SELECT count(*) AS n FROM a WHERE a.id = b.w) s ON true",
            "SELECT s.n FROM b
                 LEFT JOIN LATERAL (SELECT coalesce(sum(a.v), 0) AS n FROM a WHERE a.w = b.v) s ON true",
            "SELECT s.n FROM b LEFT JOIN (SELECT count(*) AS n FROM c) s ON true",
            "SELECT s.n FROM b LEFT JOIN LATERAL (SELECT b.v + 1 AS n) s ON true",
            "SELECT s.n FROM b LEFT JOIN LATERAL (VALUES (b.v)) s(n) ON true",
            "WITH s AS (SELECT count(*) AS n FROM c) SELECT s.n FROM b LEFT JOIN s ON true",
            "SELECT s.n FROM (SELECT count(*) AS n FROM c) s RIGHT JOIN b ON true",
            // A FULL join keeps the other side's null-extension only.
            "SELECT s.n FROM b FULL JOIN (SELECT count(*) AS n FROM c) s ON true",
        ],
    );
    assert_nullable(
        &db,
        &[
            // The aggregate's own value may be NULL.
            "SELECT s.n FROM b LEFT JOIN LATERAL (SELECT max(a.v) AS n FROM a WHERE a.id = b.w) s ON true",
            // HAVING, GROUP BY, OFFSET, LIMIT 0 may leave no row.
            "SELECT s.n FROM b
                 LEFT JOIN LATERAL (SELECT count(*) AS n FROM a HAVING count(*) > 1) s ON true",
            "SELECT s.n FROM b
                 LEFT JOIN LATERAL (SELECT count(*) AS n FROM a GROUP BY a.v) s ON true",
            "SELECT s.n FROM b LEFT JOIN (SELECT count(*) AS n FROM c OFFSET 1) s ON true",
            "SELECT s.n FROM b LEFT JOIN (SELECT count(*) AS n FROM c LIMIT 0) s ON true",
            // A set-returning select list may yield nothing.
            "SELECT s.n FROM b
                 LEFT JOIN (SELECT count(*) AS n, generate_series(1, 0) AS g FROM c) s ON true",
            // Not ON true.
            "SELECT s.n FROM b LEFT JOIN (SELECT count(*) AS n FROM c) s ON s.n > 1",
            "SELECT s.n FROM b LEFT JOIN LATERAL (SELECT b.v + 1 AS n) s ON b.v > 1",
            // A filtered FROM-less query.
            "SELECT s.n FROM b LEFT JOIN LATERAL (SELECT 1 AS n WHERE b.v > 1) s ON true",
            // The preserved side of a FULL join may be empty.
            "SELECT b.v FROM b FULL JOIN (SELECT count(*) AS n FROM c) s ON true",
        ],
    );
}

// ── Scalar subqueries that always return a row ──────────────────────────────

#[test]
fn a_scalar_subquery_along_a_foreign_key_finds_the_referenced_row() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT (SELECT a.v FROM a WHERE a.id = b.a_id) AS x FROM b",
            "SELECT (SELECT max(a.v) FROM a WHERE a.id = b.a_id) AS x FROM b",
            "SELECT (SELECT a.v FROM a WHERE b.a_id = a.id) AS x FROM b",
            // A nullable key proven non-NULL.
            "SELECT (SELECT a.v FROM a WHERE a.id = b.w) AS x FROM b WHERE b.w IS NOT NULL",
            "SELECT (SELECT p.name FROM node p WHERE p.id = n.parent_id) AS x
             FROM node n WHERE n.parent_id IS NOT NULL",
            // As a LATERAL lookup, LIMIT 1 or not.
            "SELECT s.v FROM b LEFT JOIN LATERAL (SELECT * FROM a WHERE a.id = b.a_id) s ON true",
            "SELECT s.v FROM b
                 LEFT JOIN LATERAL (SELECT a.v FROM a WHERE a.id = b.a_id LIMIT 1) s ON true",
        ],
    );
    assert_nullable(
        &db,
        &[
            "SELECT (SELECT a.v FROM a WHERE a.id = b.w) AS x FROM b",
            // Another condition may fail, the wrong columns, the wrong way.
            "SELECT (SELECT a.v FROM a WHERE a.id = b.a_id AND a.w = 1) AS x FROM b",
            "SELECT (SELECT a.v FROM a WHERE a.id = b.v) AS x FROM b",
            "SELECT (SELECT b.v FROM b WHERE b.a_id = a.id) AS x FROM a",
            // The value itself may be NULL.
            "SELECT (SELECT a.w FROM a WHERE a.id = b.a_id) AS x FROM b",
            // Row security, NOT ENFORCED, LIMIT 0, a sample.
            "SELECT (SELECT s.v FROM secret s WHERE s.id = u.secret_id) AS x FROM uses_secret u",
            "SELECT (SELECT a.v FROM a WHERE a.id = l.a_id) AS x FROM lax l",
            "SELECT (SELECT a.v FROM a WHERE a.id = b.a_id LIMIT 0) AS x FROM b",
            "SELECT (SELECT a.v FROM a TABLESAMPLE BERNOULLI (50) WHERE a.id = b.a_id) AS x FROM b",
            // The rows a DML statement writes: the snapshot need not see
            // their referenced row (one inserted by a CTE beside them).
            "INSERT INTO b VALUES (1, 1, 1) RETURNING (SELECT a.v FROM a WHERE a.id = b.a_id)",
            // Locked rows are re-fetched without re-checking the key.
            "SELECT (SELECT a.v FROM a WHERE a.id = b.a_id) AS x FROM b FOR UPDATE",
        ],
    );
}

#[test]
fn a_correlated_subquery_over_the_outer_row_s_own_table_finds_it() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT (SELECT max(b2.v) FROM b b2 WHERE b2.a_id = b.a_id) AS x FROM b",
            "SELECT (SELECT min(b2.id) FROM b b2 WHERE b2.a_id = b.a_id) AS x FROM b",
            "SELECT (SELECT a2.v FROM a a2 WHERE a2.id = a.id) AS x FROM a",
        ],
    );
    assert_nullable(
        &db,
        &[
            // A nullable column, another column, another table.
            "SELECT (SELECT max(b2.v) FROM b b2 WHERE b2.w = b.w) AS x FROM b",
            "SELECT (SELECT max(b2.v) FROM b b2 WHERE b2.a_id = b.v) AS x FROM b",
            "SELECT (SELECT max(c.v) FROM c WHERE c.id = b.id) AS x FROM b",
            // Nothing ties the subquery to the outer row: an aggregate query
            // yields a row over an empty table too
            // (`SELECT count(*), (SELECT 1 FROM b b2 LIMIT 1) FROM b`).
            "SELECT (SELECT 1 FROM b b2 LIMIT 1) AS x FROM b",
            // The outer row may be null-extended.
            "SELECT (SELECT max(b2.v) FROM b b2 WHERE b2.a_id = b.a_id) AS x
             FROM a LEFT JOIN b ON b.w = a.id",
            // An inheritance parent's rows aren't all an ONLY scan's.
            "SELECT (SELECT max(p2.v) FROM ONLY inh_parent p2 WHERE p2.id = p.id) AS x
             FROM inh_parent p",
        ],
    );
}

#[test]
fn a_scalar_subquery_over_one_row_sources_returns_a_row() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT (SELECT row_to_json(x) FROM (SELECT a.id, a.w) x) AS j FROM a",
            "SELECT (SELECT x.id FROM (SELECT a.id) x) AS j FROM a",
            "SELECT (SELECT v FROM (VALUES (1)) z(v)) AS x",
            "SELECT (SELECT 1 WHERE true) AS x",
            "WITH s AS (SELECT count(*) AS n FROM c) SELECT (SELECT n FROM s) AS n",
            "SELECT (SELECT x FROM generate_series(1, 10) x LIMIT 1) AS x",
        ],
    );
    assert_nullable(
        &db,
        &[
            "SELECT (SELECT x.w FROM (SELECT a.w) x) AS j FROM a",
            "SELECT (SELECT 1 WHERE false) AS x",
            "SELECT (SELECT x FROM generate_series(1, 0) x LIMIT 1) AS x",
            "SELECT (SELECT x FROM generate_series(1, 10) x OFFSET 20) AS x",
            "WITH s AS (SELECT count(*) AS n FROM c GROUP BY v) SELECT (SELECT n FROM s) AS n",
        ],
    );
}

// ── Foreign keys through subqueries, CTEs, views and join trees ─────────────

#[test]
fn a_foreign_key_child_read_through_a_derived_relation_still_holds() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "WITH ab AS (SELECT * FROM b WHERE v > 0) SELECT a.v FROM ab LEFT JOIN a ON a.id = ab.a_id",
            "SELECT a.v FROM (SELECT a_id FROM b) s LEFT JOIN a ON a.id = s.a_id",
            "SELECT a.v FROM vb LEFT JOIN a ON a.id = vb.a_id",
            "SELECT a.v FROM (b JOIN c ON c.b_id = b.id) AS j LEFT JOIN a ON a.id = j.a_id",
        ],
    );
    assert_nullable(
        &db,
        &[
            // Not passed through as is.
            "SELECT a.v FROM (SELECT a_id + 0 AS a_id FROM b) s LEFT JOIN a ON a.id = s.a_id",
            "SELECT a.v FROM (SELECT a_id FROM b UNION ALL SELECT 5) s LEFT JOIN a ON a.id = s.a_id",
            // A data-modifying CTE's rows aren't the snapshot's.
            "WITH nb AS (INSERT INTO b VALUES (1, 1, 1) RETURNING *)
             SELECT a.v FROM nb LEFT JOIN a ON a.id = nb.a_id",
        ],
    );
}

#[test]
fn a_foreign_key_parent_read_through_a_derived_relation_keeping_its_rows_still_holds() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT va.v FROM b LEFT JOIN va ON va.id = b.a_id",
            "SELECT s.v FROM b LEFT JOIN (SELECT id, v FROM a) s ON s.id = b.a_id",
            "WITH s AS (SELECT * FROM a) SELECT s.v FROM b LEFT JOIN s ON s.id = b.a_id",
        ],
    );
    assert_nullable(
        &db,
        &[
            // Filtered or limited: the referenced row may be gone.
            "SELECT s.v FROM b LEFT JOIN va_filtered s ON s.id = b.a_id",
            "SELECT s.v FROM b LEFT JOIN (SELECT id, v FROM a WHERE w > 0) s ON s.id = b.a_id",
            "SELECT s.v FROM b LEFT JOIN (SELECT id, v FROM a LIMIT 1) s ON s.id = b.a_id",
            "SELECT s.v FROM b LEFT JOIN (SELECT DISTINCT ON (v) id, v FROM a) s ON s.id = b.a_id",
        ],
    );
}

#[test]
fn a_foreign_key_into_a_join_tree_keeping_the_parent_s_rows_holds() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT a.v FROM c LEFT JOIN (b JOIN a ON a.id = b.a_id) ON b.id = c.b_id",
            "SELECT a.v FROM c LEFT JOIN (b LEFT JOIN a ON a.id = b.a_id) ON b.id = c.b_id",
        ],
    );
    assert_nullable(
        &db,
        &[
            // The inner join may drop the referenced row.
            "SELECT b.v FROM c LEFT JOIN (b JOIN a ON a.id = b.w) ON b.id = c.b_id",
            "SELECT b.v FROM c LEFT JOIN (b JOIN a ON a.w = b.v) ON b.id = c.b_id",
        ],
    );
}

#[test]
fn foreign_key_equalities_may_cast_to_the_same_type_or_be_not_distinct() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id::int",
            "SELECT a.v FROM b LEFT JOIN a ON a.id IS NOT DISTINCT FROM b.a_id",
        ],
    );
    assert_nullable(
        &db,
        &[
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id::int8",
            // A NULL key matches nothing, NOT DISTINCT or not.
            "SELECT a.v FROM b LEFT JOIN a ON a.id IS NOT DISTINCT FROM b.w",
            "SELECT a.v FROM b LEFT JOIN a ON a.id IS DISTINCT FROM b.a_id",
        ],
    );
}

#[test]
fn a_self_join_on_the_row_s_own_columns_finds_the_row() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT y.v FROM a x LEFT JOIN a y ON y.id = x.id",
            "SELECT y.id FROM b x LEFT JOIN b y ON y.a_id = x.a_id",
        ],
    );
    assert_nullable(
        &db,
        &[
            "SELECT y.v FROM a x LEFT JOIN a y ON y.w = x.w",
            "SELECT y.v FROM a x LEFT JOIN a y ON y.id = x.v",
            "SELECT y.v FROM a x LEFT JOIN a y TABLESAMPLE BERNOULLI (50) ON y.id = x.id",
            "SELECT y.v FROM inh_parent x LEFT JOIN ONLY inh_parent y ON y.id = x.id",
        ],
    );
}

// ── FULL joins ───────────────────────────────────────────────────────────────

#[test]
fn coalesce_over_both_sides_of_a_full_join_is_not_null() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT COALESCE(x.id, y.id) AS k FROM a x FULL JOIN c y ON x.id = y.id",
            "SELECT COALESCE(x.v, y.v) AS k FROM a x FULL JOIN c y ON x.v = y.v",
            "SELECT GREATEST(x.v, y.v) AS k FROM a x FULL JOIN c y ON x.v = y.v",
        ],
    );
    assert_nullable(
        &db,
        &[
            "SELECT COALESCE(x.w, y.id) AS k FROM a x FULL JOIN c y ON x.id = y.id",
            "SELECT COALESCE(x.id, x.v) AS k FROM a x FULL JOIN c y ON x.id = y.id",
            // A join above null-extends both.
            "SELECT COALESCE(x.id, y.id) AS k
             FROM (a x FULL JOIN c y ON x.id = y.id) RIGHT JOIN b ON b.v > 1",
            // A grouping set may NULL one of them out.
            "SELECT COALESCE(x.id, y.id) AS k FROM a x FULL JOIN c y ON x.id = y.id
             GROUP BY GROUPING SETS ((x.id), (y.id))",
        ],
    );
}

// ── Quals on a derived relation's columns reduce the joins inside ───────────

#[test]
fn a_qual_on_a_column_passed_through_proves_its_row_is_there() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT s.v FROM (SELECT a.v, a.id FROM b LEFT JOIN a ON a.id = b.w) s WHERE s.id > 0",
            "SELECT v FROM vbw WHERE aid > 0",
            "WITH s AS (SELECT a.v, a.id FROM b LEFT JOIN a ON a.id = b.w) SELECT v FROM s WHERE id > 0",
        ],
    );
    assert_nullable(
        &db,
        &[
            "SELECT s.v FROM (SELECT a.v, b.id FROM b LEFT JOIN a ON a.id = b.w) s WHERE s.id > 0",
            "SELECT s.v FROM (SELECT a.v, a.id FROM b LEFT JOIN a ON a.id = b.w) s
             WHERE s.id IS NULL OR s.id > 0",
            // Another reference to the CTE is another scan.
            "WITH s AS (SELECT a.v, a.id FROM b LEFT JOIN a ON a.id = b.w)
             SELECT s2.v FROM s s1, s s2 WHERE s1.id > 0",
        ],
    );
}

#[test]
fn a_lateral_subquery_s_where_and_a_strict_function_s_arguments_hold_in_its_rows() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w, LATERAL (SELECT 1 FROM c WHERE c.id = a.w) s",
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w JOIN LATERAL (SELECT a.w AS z) s ON s.z > 0",
            "SELECT a.data FROM a CROSS JOIN LATERAL jsonb_array_elements(a.data -> 'items') e",
            "SELECT b.w FROM b CROSS JOIN LATERAL generate_series(1, b.w) g",
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w CROSS JOIN LATERAL generate_series(1, a.w) g",
        ],
    );
    assert_nullable(
        &db,
        &[
            // On the nullable side, the function or subquery may be absent.
            "SELECT b.w FROM b LEFT JOIN LATERAL generate_series(1, b.w) g ON true",
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w
                 LEFT JOIN LATERAL (SELECT 1 FROM c WHERE c.id = a.w) s ON true",
            // An aggregate yields its row whatever WHERE says.
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w,
                 LATERAL (SELECT count(*) FROM c WHERE c.id = a.w) s",
            // Several functions pad each other's rows.
            "SELECT b.w FROM b CROSS JOIN LATERAL ROWS FROM (generate_series(1, b.w), generate_series(1, 2)) g",
        ],
    );
}

#[test]
fn a_row_comparison_is_strict_in_its_columns() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w WHERE (a.w, b.v) = (1, 2)",
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w WHERE (a.w, b.v) < (1, 2)",
        ],
    );
    assert_nullable(
        &db,
        &[
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w WHERE (b.v, a.w) < (1, 2)",
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w WHERE (a.w, b.v) <> (1, 2)",
            "SELECT a.v FROM b LEFT JOIN a ON a.id = b.w WHERE NOT ((a.w, b.v) = (1, 2))",
        ],
    );
}

#[test]
fn an_aliased_join_keeps_the_parent_s_rows_only_if_its_joins_do() {
    let db = setup();
    assert_not_null(
        &db,
        &["SELECT j.bv FROM c
               LEFT JOIN (b LEFT JOIN a ON a.id = b.w) AS j(bid, ba_id, bv)
               ON j.bid = c.b_id"],
    );
    assert_nullable(
        &db,
        &["SELECT j.bv FROM c
               LEFT JOIN (b JOIN a ON a.w = b.v) AS j(bid, ba_id, bv)
               ON j.bid = c.b_id"],
    );
}
