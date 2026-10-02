//! Reasoning with CHECK constraints and what PG implies like them: the
//! shapes a constraint is read in (NOT, an iff between conditions, IS
//! DISTINCT FROM, CASE, BETWEEN, `coalesce(…) IS NOT NULL`), the facts a
//! query adds (`<>`, IN lists, `= ANY`, orderings, a simple CASE's arms),
//! constraints combined with each other and split into cases, partition
//! bounds, `MATCH FULL` foreign keys, and a CASE without ELSE whose WHENs
//! cover every case. Every NOT NULL below is checked against PostgreSQL 18
//! by the pg_sanity soundness oracle, over rows that satisfy the
//! constraints; the near misses stay nullable.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE st AS ENUM ('a', 'b');
         CREATE TABLE c1 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK ((kind = 'a') = (a_id IS NOT NULL)));
         CREATE TABLE c5 (id int PRIMARY KEY, status text NOT NULL, done_at int,
             CHECK ((status = 'a') <> (done_at IS NULL)));
         CREATE TABLE c11 (id int PRIMARY KEY, a_id int, b_id int,
             CHECK ((a_id IS NULL) <> (b_id IS NULL)));
         CREATE TABLE p9 (id int PRIMARY KEY, deleted_at int, deleted_by int,
             CHECK ((deleted_at IS NULL) = (deleted_by IS NULL)));
         CREATE TABLE c4 (id int PRIMARY KEY, lvl int NOT NULL, a_id int,
             CHECK (lvl < 2 OR a_id IS NOT NULL));
         CREATE TABLE par (id int NOT NULL, a int, CHECK (a IS NOT NULL OR id < 0));
         CREATE TABLE c9 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK (NOT (kind = 'a' AND a_id IS NULL)));
         CREATE TABLE c3 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK (kind IS DISTINCT FROM 'a' OR a_id IS NOT NULL));
         CREATE TABLE c2 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK (CASE WHEN kind = 'a' THEN a_id IS NOT NULL ELSE true END));
         CREATE TABLE p3 (id int PRIMARY KEY, a int, b int, CHECK (coalesce(a, b) IS NOT NULL));
         CREATE TABLE c6 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK (kind <> 'a' OR a_id IS NOT NULL));
         CREATE TABLE c10 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK (kind = 'a' AND a_id IS NOT NULL OR kind = 'b' AND a_id IS NULL));
         CREATE TABLE c12 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK (kind NOT IN ('a', 'b') OR a_id IS NOT NULL));
         CREATE TABLE p5 (id int PRIMARY KEY, kind text NOT NULL CHECK (kind IN ('a', 'b')),
             a_id int, b_id int,
             CHECK (kind <> 'a' OR a_id IS NOT NULL), CHECK (kind <> 'b' OR b_id IS NOT NULL));
         CREATE TABLE c14 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK (kind = 'a'), CHECK (kind <> 'a' OR a_id IS NOT NULL));
         CREATE TABLE cn (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK (kind IN ('a', NULL) OR a_id IS NOT NULL));
         CREATE TABLE nk (id int PRIMARY KEY, kind text, a_id int,
             CHECK (kind <> 'a' OR a_id IS NOT NULL));
         CREATE TABLE dt (id int PRIMARY KEY, d date NOT NULL, x int,
             CHECK (d <> 'today' OR x IS NOT NULL), CHECK (d NOT IN ('today', 'tomorrow') OR x IS NOT NULL));
         CREATE TABLE ci (id int PRIMARY KEY, kind text COLLATE \"C\" NOT NULL, a_id int,
             CHECK (kind <> 'a' OR a_id IS NOT NULL));
         CREATE TABLE p7 (id int PRIMARY KEY, s st NOT NULL, done_at int);
         CREATE TABLE p7n (id int PRIMARY KEY, s st, done_at int);
         CREATE TABLE p8 (id int PRIMARY KEY, active boolean NOT NULL, x int);
         CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, b int, flag bool NOT NULL);
         CREATE TABLE pr (id int, d date, v int) PARTITION BY RANGE (d);
         CREATE TABLE pr_2000 PARTITION OF pr FOR VALUES FROM ('2000-01-01') TO ('2001-01-01');
         CREATE TABLE pr_2001 PARTITION OF pr FOR VALUES FROM ('2001-01-01') TO ('2002-01-01');
         CREATE TABLE pl (id int, kind text, v int) PARTITION BY LIST (kind);
         CREATE TABLE pl_a PARTITION OF pl FOR VALUES IN ('a');
         CREATE TABLE pl_b PARTITION OF pl FOR VALUES IN ('b', 'c');
         CREATE TABLE pl_n PARTITION OF pl FOR VALUES IN (NULL);
         CREATE TABLE pl2 (id int, kind text, v int, CHECK (kind <> 'a' OR v IS NOT NULL))
             PARTITION BY LIST (kind);
         CREATE TABLE pl2_a PARTITION OF pl2 FOR VALUES IN ('a');
         CREATE TABLE pl2_d PARTITION OF pl2 DEFAULT;
         CREATE TABLE pr2 (id int, a int, b int) PARTITION BY RANGE (a, b);
         CREATE TABLE pr2_1 PARTITION OF pr2 FOR VALUES FROM (0, 0) TO (10, 10);
         CREATE TABLE pe (id int, d date) PARTITION BY RANGE ((d + 1));
         CREATE TABLE pe_1 PARTITION OF pe FOR VALUES FROM ('2000-01-01') TO ('2002-01-01');
         CREATE TABLE ph (id int, k int) PARTITION BY HASH (k);
         CREATE TABLE ph_0 PARTITION OF ph FOR VALUES WITH (MODULUS 1, REMAINDER 0);
         CREATE TABLE p2 (x int, y int, v int NOT NULL, PRIMARY KEY (x, y));
         CREATE TABLE c2f (id int PRIMARY KEY, x int, y int,
             FOREIGN KEY (x, y) REFERENCES p2 MATCH FULL);
         CREATE TABLE c2s (id int PRIMARY KEY, x int, y int, FOREIGN KEY (x, y) REFERENCES p2);",
    )
    .unwrap();
    db
}

/// Each output column's nullability (`true` = nullable).
#[track_caller]
fn assert_nullable(db: &PgCatalog, sql: &str, expected: &[(&str, bool)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, bool)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    assert_eq!(actual, expected, "nullability mismatch for `{sql}`");
}

#[track_caller]
fn nn(db: &PgCatalog, sql: &str) {
    let s = db.analyze(sql).unwrap();
    assert!(
        s.columns.iter().all(|c| !c.nullable),
        "expected NOT NULL for `{sql}`: {:?}",
        s.columns
    );
}

#[track_caller]
fn nullable(db: &PgCatalog, sql: &str) {
    let s = db.analyze(sql).unwrap();
    assert!(
        s.columns.iter().all(|c| c.nullable),
        "expected nullable for `{sql}`: {:?}",
        s.columns
    );
}

// ── Constraint shapes ────────────────────────────────────────────────────────

#[test]
fn a_boolean_equality_between_conditions_is_an_iff_or_a_xor() {
    let db = setup();
    nn(&db, "SELECT a_id FROM c1 WHERE kind = 'a'");
    nn(&db, "SELECT done_at FROM c5 WHERE status = 'a'");
    nn(&db, "SELECT b_id FROM c11 WHERE a_id IS NULL");
    nn(
        &db,
        "SELECT deleted_by FROM p9 WHERE deleted_at IS NOT NULL",
    );
    // A strict comparison proves its column non-NULL.
    nn(&db, "SELECT deleted_by FROM p9 WHERE deleted_at < 2");
    // The other side of the iff: the column is NULL, not unknown.
    nullable(&db, "SELECT a_id FROM c1 WHERE kind = 'b'");
    nullable(&db, "SELECT a_id FROM c1");
    nullable(&db, "SELECT b_id FROM c11 WHERE a_id IS NOT NULL");
    nullable(&db, "SELECT b_id FROM c11");
}

#[test]
fn not_is_distinct_from_case_and_coalesce_in_a_constraint_are_read() {
    let db = setup();
    nn(&db, "SELECT a_id FROM c9 WHERE kind = 'a'");
    nn(&db, "SELECT a_id FROM c3 WHERE kind = 'a'");
    nn(&db, "SELECT a_id FROM c2 WHERE kind = 'a'");
    nn(&db, "SELECT coalesce(a, b) AS c FROM p3");
    nullable(&db, "SELECT a_id FROM c9 WHERE kind = 'b'");
    nullable(&db, "SELECT a_id FROM c3 WHERE kind = 'b'");
    nullable(&db, "SELECT a_id FROM c2 WHERE kind = 'b'");
    nullable(&db, "SELECT a FROM p3");
}

#[test]
fn an_ordering_refutes_an_ordering_alternative() {
    let db = setup();
    nn(&db, "SELECT a_id FROM c4 WHERE lvl >= 2");
    nn(&db, "SELECT a_id FROM c4 WHERE lvl = 2");
    nn(&db, "SELECT a_id FROM c4 WHERE 2 <= lvl");
    nn(&db, "SELECT a_id FROM c4 WHERE lvl BETWEEN 2 AND 5");
    nn(&db, "SELECT a FROM par WHERE id >= 0");
    nn(&db, "SELECT a FROM par WHERE id > -1");
    // Ranges that overlap the alternative.
    nullable(&db, "SELECT a_id FROM c4 WHERE lvl >= 1");
    nullable(&db, "SELECT a_id FROM c4 WHERE lvl > 0");
    nullable(&db, "SELECT a_id FROM c4 WHERE lvl <= 2");
    nullable(&db, "SELECT a FROM par WHERE id > -2");
}

// ── Query facts ──────────────────────────────────────────────────────────────

#[test]
fn inequalities_in_lists_and_any_arrays_refute_alternatives() {
    let db = setup();
    nn(&db, "SELECT a_id FROM c10 WHERE kind <> 'b'");
    nn(&db, "SELECT a_id FROM c12 WHERE kind IN ('a', 'b')");
    nn(&db, "SELECT a_id FROM c6 WHERE kind IN ('a')");
    nn(&db, "SELECT a_id FROM c6 WHERE kind = ANY ('{a}')");
    nn(&db, "SELECT a_id FROM c6 WHERE kind = ANY (ARRAY['a'])");
    nn(&db, "SELECT a_id FROM c6 WHERE kind = 'a' OR kind = 'a'");
    nn(&db, "SELECT a_id FROM c12 WHERE kind = 'a' OR kind = 'b'");
    nn(
        &db,
        "SELECT CASE kind WHEN 'a' THEN a_id ELSE 0 END AS v FROM c6",
    );
    // A value outside the list, or a list too wide.
    nullable(&db, "SELECT a_id FROM c12 WHERE kind IN ('a', 'c')");
    nullable(&db, "SELECT a_id FROM c6 WHERE kind IN ('a', 'b')");
    nullable(&db, "SELECT a_id FROM c6 WHERE kind = ANY ('{a,b}')");
    nullable(&db, "SELECT a_id FROM c10 WHERE kind <> 'a'");
    nullable(
        &db,
        "SELECT CASE kind WHEN 'b' THEN a_id ELSE 0 END AS v FROM c6",
    );
    // A NULL in the list keeps IN from being FALSE in the constraint.
    nullable(&db, "SELECT a_id FROM cn WHERE kind = 'b'");
}

#[test]
fn a_nullable_column_or_a_collated_literal_proves_nothing() {
    let db = setup();
    nn(&db, "SELECT a_id FROM nk WHERE kind = 'a'");
    nn(
        &db,
        "SELECT a_id FROM nk WHERE kind IN ('a', 'b') AND kind <> 'b'",
    );
    // Past `kind <> 'a'` not TRUE, `kind` is `'a'` — or NULL, which
    // passes the CHECK too.
    nn(
        &db,
        "SELECT CASE WHEN kind <> 'a' THEN 0 ELSE a_id END AS v FROM c6",
    );
    nullable(
        &db,
        "SELECT CASE WHEN kind <> 'a' THEN 0 ELSE a_id END AS v FROM nk",
    );
    nn(&db, "SELECT a_id FROM ci WHERE kind = 'a'");
    // Another collation's equality says nothing of the column's.
    nullable(
        &db,
        "SELECT a_id FROM ci WHERE kind = 'a' COLLATE \"POSIX\"",
    );
    // `'today'` is read when the constraint is created, and again when
    // the query is: the same text, not the same value.
    nullable(&db, "SELECT x FROM dt WHERE d = 'today'");
    nullable(&db, "SELECT x FROM dt WHERE d IN ('today', 'tomorrow')");
}

#[test]
fn constraints_combine_and_split_into_cases() {
    let db = setup();
    nn(
        &db,
        "SELECT CASE WHEN kind = 'a' THEN a_id ELSE b_id END AS r FROM p5",
    );
    nn(&db, "SELECT coalesce(a_id, b_id) AS r FROM p5");
    nn(&db, "SELECT a_id FROM c14");
    nullable(&db, "SELECT a_id FROM p5");
    nullable(&db, "SELECT coalesce(a_id, 0 + NULL) AS r FROM p5");
    nullable(
        &db,
        "SELECT CASE WHEN kind = 'b' THEN a_id ELSE b_id END AS r FROM p5",
    );
}

// ── CASE without ELSE ────────────────────────────────────────────────────────

#[test]
fn a_case_whose_whens_cover_every_case_never_reaches_its_else() {
    let db = setup();
    nn(
        &db,
        "SELECT CASE s WHEN 'a' THEN 1 WHEN 'b' THEN 2 END AS r FROM p7",
    );
    nn(
        &db,
        "SELECT CASE WHEN s = 'a' THEN 1 WHEN s = 'b' THEN 2 END AS r FROM p7",
    );
    nn(
        &db,
        "SELECT CASE WHEN active THEN 1 WHEN NOT active THEN 2 END AS r FROM p8",
    );
    nn(
        &db,
        "SELECT CASE kind WHEN 'a' THEN 1 WHEN 'b' THEN 2 END AS r FROM p5",
    );
    nn(
        &db,
        "SELECT CASE WHEN kind = 'a' THEN 1 WHEN kind = 'b' THEN 2 END AS r FROM p5",
    );
    nn(
        &db,
        "SELECT CASE WHEN b IS NULL THEN 1 WHEN b IS NOT NULL THEN 2 END AS r FROM t",
    );
    nn(
        &db,
        "SELECT CASE WHEN b IS NOT NULL THEN b WHEN b IS NULL THEN 0 END AS r FROM t",
    );
    nn(
        &db,
        "SELECT CASE WHEN flag THEN 'a' WHEN NOT flag THEN 'b' END AS r FROM t",
    );
    nn(
        &db,
        "SELECT CASE WHEN a > 0 THEN 1 WHEN a <= 0 THEN 2 END AS r FROM t",
    );
    nn(
        &db,
        "SELECT CASE WHEN a > 0 THEN 'pos' END AS r FROM t WHERE a > 0",
    );
    // A value no WHEN covers: a NULL column, a missing label or value.
    nullable(
        &db,
        "SELECT CASE s WHEN 'a' THEN 1 WHEN 'b' THEN 2 END AS r FROM p7n",
    );
    nullable(&db, "SELECT CASE s WHEN 'a' THEN 1 END AS r FROM p7");
    nullable(&db, "SELECT CASE kind WHEN 'a' THEN 1 END AS r FROM p5");
    nullable(
        &db,
        "SELECT CASE WHEN a > 0 THEN 1 WHEN a < 0 THEN 2 END AS r FROM t",
    );
    nullable(
        &db,
        "SELECT CASE WHEN b > 0 THEN 1 WHEN b <= 0 THEN 2 END AS r FROM t",
    );
    nullable(
        &db,
        "SELECT CASE WHEN a > 0 THEN 'pos' END AS r FROM t WHERE a >= 0",
    );
    // A NULL-extended row breaks no constraint and holds no label.
    nullable(
        &db,
        "SELECT CASE p7.s WHEN 'a' THEN 1 WHEN 'b' THEN 2 END AS r \
         FROM t LEFT JOIN p7 ON p7.id = t.id",
    );
    nullable(
        &db,
        "SELECT CASE kind WHEN 'a' THEN 1 WHEN 'b' THEN 2 END AS r \
         FROM t LEFT JOIN p5 ON p5.id = t.id",
    );
    // A grouping set nulls the column out.
    nullable(
        &db,
        "SELECT CASE s WHEN 'a' THEN 1 WHEN 'b' THEN 2 END AS r FROM p7 GROUP BY ROLLUP (s)",
    );
    // An unreachable ELSE doesn't count, whatever it is.
    nn(
        &db,
        "SELECT CASE WHEN flag THEN 1 WHEN NOT flag THEN 2 ELSE b END AS r FROM t",
    );
}

// ── DML ──────────────────────────────────────────────────────────────────────

#[test]
fn what_where_says_of_a_column_an_update_sets_is_not_kept() {
    let db = setup();
    // RETURNING reads the new row: `kind` is `'b'` there.
    nullable(
        &db,
        "UPDATE c6 SET kind = 'b' WHERE kind = 'a' RETURNING a_id",
    );
    nullable(
        &db,
        "UPDATE c6 SET kind = 'b' WHERE kind IN ('a') RETURNING a_id",
    );
    nullable(&db, "UPDATE c4 SET lvl = 1 WHERE lvl >= 2 RETURNING a_id");
    nullable(
        &db,
        "UPDATE p8 SET active = false WHERE active RETURNING CASE WHEN active THEN 1 END AS r",
    );
    nn(
        &db,
        "UPDATE p8 SET x = 1 WHERE active RETURNING CASE WHEN active THEN 1 END AS r",
    );
}

// ── Partitions ───────────────────────────────────────────────────────────────

#[test]
fn a_partition_bound_keeps_its_key_non_null() {
    let db = setup();
    nn(&db, "SELECT d FROM pr");
    nn(&db, "SELECT d FROM pr_2000");
    nn(&db, "SELECT kind FROM pl_a");
    nn(&db, "SELECT kind FROM pl_b");
    nn(&db, "SELECT a, b FROM pr2");
    nn(&db, "SELECT a, b FROM pr2_1");
    nn(&db, "SELECT v FROM pl2_a");
    nn(&db, "SELECT d FROM pe_1");
    // A NULL partition, a default one, a hash one hold NULL keys.
    nullable(&db, "SELECT kind FROM pl");
    nullable(&db, "SELECT kind FROM pl_n");
    nullable(&db, "SELECT kind FROM pl2");
    nullable(&db, "SELECT kind FROM pl2_d");
    nullable(&db, "SELECT v FROM pl2_d");
    nullable(&db, "SELECT k FROM ph");
    nullable(&db, "SELECT k FROM ph_0");
    nullable(&db, "SELECT v FROM pr");
}

// ── MATCH FULL ───────────────────────────────────────────────────────────────

#[test]
fn a_match_full_foreign_key_has_all_or_none_of_its_columns() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT p2.v FROM c2f LEFT JOIN p2 ON p2.x = c2f.x AND p2.y = c2f.y \
         WHERE c2f.x IS NOT NULL",
        &[("v", false)],
    );
    nn(&db, "SELECT y FROM c2f WHERE x IS NOT NULL");
    nn(&db, "SELECT y FROM c2f WHERE x > 0");
    nullable(&db, "SELECT y FROM c2f");
    // MATCH SIMPLE allows a mix.
    nullable(&db, "SELECT y FROM c2s WHERE x IS NOT NULL");
    assert_nullable(
        &db,
        "SELECT p2.v FROM c2s LEFT JOIN p2 ON p2.x = c2s.x AND p2.y = c2s.y \
         WHERE c2s.x IS NOT NULL",
        &[("v", true)],
    );
}
