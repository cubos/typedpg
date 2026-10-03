//! What conditions and constraints prove, kept to what they really say:
//! comparisons and `num_nulls` only when they are the built-in ones (a
//! user-defined `=` may hold for different values), constants only as
//! the type they end up as (a type modifier truncates or rounds, a float
//! rounds a large integer), stored expressions in step with the renames
//! PG keeps by attnum or OID, foreign keys only while every partition
//! enforces them, an aggregate's row from no input, grouping sets that
//! null a column out after the row's constraints held, window calls that
//! read other rows, and expression facts only for expressions evaluated
//! the same way twice. Every NOT NULL below is checked against PostgreSQL
//! 18 by the pg_sanity soundness oracle; every nullable one returns NULL
//! there for some rows the schema allows.

use crate::common::*;

/// Each output column's nullability (`true` = nullable).
#[track_caller]
fn assert_nullable(db: &PgCatalog, sql: &str, expected: &[(&str, bool)]) {
    let s = db.analyze(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    let actual: Vec<(&str, bool)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    assert_eq!(actual, expected, "nullability mismatch for `{sql}`");
}

/// The single output column's array element nullability.
#[track_caller]
fn elements(db: &PgCatalog, sql: &str) -> Option<bool> {
    let s = db.analyze(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    match &s.columns[0].pg_type {
        Type::Array {
            element_nullable, ..
        } => *element_nullable,
        other => panic!("not an array: {other:?}"),
    }
}

fn catalog(schema: &str) -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(schema).unwrap();
    db
}

// ── Built-in comparisons and num_nulls only ─────────────────────────────────

#[test]
fn a_user_defined_equality_proves_no_value() {
    // An exact-signature `=` beats the built-in enum / text one: its
    // result says nothing of the values (NULL for equal ones here).
    let db = catalog(
        "CREATE TYPE mood AS ENUM ('sad', 'ok');
         CREATE FUNCTION mood_eq(mood, mood) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
             AS $$ SELECT NULL::bool $$;
         CREATE OPERATOR = (LEFTARG = mood, RIGHTARG = mood, FUNCTION = mood_eq);
         CREATE TABLE m (id int PRIMARY KEY, md mood NOT NULL);
         CREATE DOMAIN code AS text;
         CREATE FUNCTION code_eq(code, code) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
             AS $$ SELECT CASE WHEN $1::text = $2::text THEN true
                               WHEN lower($1::text) = lower($2::text) THEN NULL
                               ELSE false END $$;
         CREATE OPERATOR = (LEFTARG = code, RIGHTARG = code, FUNCTION = code_eq);
         CREATE TABLE c (id int PRIMARY KEY, kind code NOT NULL, CHECK (kind = 'a'));
         CREATE FUNCTION weird_eq(numeric, int) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
             AS 'SELECT $1 >= $2';
         CREATE OPERATOR = (LEFTARG = numeric, RIGHTARG = int, FUNCTION = weird_eq);
         CREATE TABLE tn (n numeric, d int, CHECK (n <> 1 OR d IS NOT NULL));
         CREATE TABLE tn2 (n numeric, d int, CHECK (n = 1 OR d IS NOT NULL));
         CREATE TYPE st AS ENUM ('x', 'y');
         CREATE TABLE s (id int PRIMARY KEY, k st NOT NULL);",
    );
    for sql in [
        "SELECT CASE WHEN md = 'sad' THEN 1 WHEN md = 'ok' THEN 2 END AS x FROM m",
        "SELECT CASE md WHEN 'sad' THEN 1 WHEN 'ok' THEN 2 END AS x FROM m",
        "SELECT CASE WHEN kind = 'a' THEN 1 END AS x FROM c",
        "SELECT d AS x FROM tn WHERE n = 1",
        "SELECT CASE n WHEN 1 THEN d ELSE 0 END AS x FROM tn",
        "SELECT d AS x FROM tn2 WHERE n <> 1",
    ] {
        assert_nullable(&db, sql, &[("x", true)]);
    }
    // The built-in enum `=`: every label covered.
    assert_nullable(
        &db,
        "SELECT CASE WHEN k = 'x' THEN 1 WHEN k = 'y' THEN 2 END AS x FROM s",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "SELECT CASE k WHEN 'x' THEN 1 WHEN 'y' THEN 2 END AS x FROM s",
        &[("x", false)],
    );
    // The built-in `numeric = numeric` (the constant written as one).
    assert_nullable(&db, "SELECT d AS x FROM tn WHERE n = 1.0", &[("x", true)]);
    assert_nullable(
        &db,
        "SELECT d AS x FROM tn WHERE n = 1::numeric",
        &[("x", false)],
    );
}

#[test]
fn a_user_defined_num_nonnulls_counts_nothing() {
    let db = catalog(
        "CREATE FUNCTION num_nonnulls(int, int) RETURNS int LANGUAGE sql IMMUTABLE
             AS 'SELECT 2';
         CREATE FUNCTION num_nulls(int, int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 0';
         CREATE FUNCTION num_nonnulls(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 1';
         CREATE TABLE t (id int PRIMARY KEY, a int, b int, CHECK (num_nonnulls(a, b) = 2));
         CREATE TABLE q (id int PRIMARY KEY, a int, b int, CHECK (num_nulls(a, b) = 0));
         CREATE TABLE r (id int PRIMARY KEY, a int, b int,
             CHECK (pg_catalog.num_nonnulls(a, b) = 2));
         CREATE TABLE u (a int, b int);",
    );
    assert_nullable(&db, "SELECT a, b FROM t", &[("a", true), ("b", true)]);
    assert_nullable(&db, "SELECT coalesce(a, b) AS c FROM q", &[("c", true)]);
    assert_nullable(
        &db,
        "SELECT a FROM u WHERE num_nonnulls(a) = 1",
        &[("a", true)],
    );
    assert_nullable(
        &db,
        "SELECT coalesce(a, b) AS c FROM u WHERE num_nulls(a, b) < 2",
        &[("c", true)],
    );
    // `pg_catalog.num_nonnulls` is the built-in, whatever else exists.
    assert_nullable(&db, "SELECT a, b FROM r", &[("a", false), ("b", false)]);
    assert_nullable(
        &db,
        "SELECT a FROM u WHERE pg_catalog.num_nonnulls(a, b) = 2",
        &[("a", false)],
    );
}

// ── Constants as the type they end up as ────────────────────────────────────

#[test]
fn a_type_modifier_changes_a_cast_constant() {
    let db = catalog(
        "CREATE TABLE tv (c varchar(10), d int, CHECK (c <> 'abcdef' OR d IS NOT NULL));
         CREATE TABLE tn (n numeric, d int, CHECK (n <> 15 OR d IS NOT NULL));
         CREATE TABLE t (id int PRIMARY KEY, k varchar NOT NULL, c char(2) NOT NULL);
         CREATE TABLE u (id int PRIMARY KEY, k varchar NOT NULL CHECK (k = 'abc'::varchar(2)),
             v int);
         CREATE DOMAIN short AS varchar(2);
         CREATE TABLE w (id int PRIMARY KEY, k short NOT NULL, v int,
             CHECK (k <> 'abc'::short OR v IS NOT NULL));
         CREATE TABLE tn2 (n numeric, d int);",
    );
    for sql in [
        // 'abcdef'::varchar(3) is 'abc'.
        "SELECT d AS x FROM tv WHERE c = 'abcdef'::varchar(3)",
        // 15::numeric(1,-1) is 20.
        "SELECT d AS x FROM tn WHERE n = 15::numeric(1,-1)",
        "SELECT CASE WHEN n = 15::numeric(1,-1) THEN d END AS x FROM tn",
        "SELECT n AS x FROM tn2 WHERE coalesce(n, 15::numeric(1,-1)) <> 15",
        "SELECT CASE WHEN coalesce(n, 15::numeric(1,-1)) = 15 THEN 1 ELSE n END AS x FROM tn2",
        "SELECT CASE WHEN k = 'abc' THEN 1 END AS x FROM t WHERE k = 'abc'::varchar(2)",
        "SELECT CASE WHEN c = 'ab' THEN 1 END AS x FROM t WHERE c = 'ab'::char(1)",
        "SELECT CASE WHEN k = 'abc' THEN 1 END AS x FROM u",
        // A domain's modifier applies too: 'abc'::short is 'ab'.
        "SELECT v AS x FROM w WHERE k = 'abc'",
    ] {
        assert_nullable(&db, sql, &[("x", true)]);
    }
    // Without a modifier the constant is as written.
    assert_nullable(
        &db,
        "SELECT d AS x FROM tv WHERE c = 'abcdef'::varchar",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "SELECT d AS x FROM tn WHERE n = 15::numeric",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "SELECT n AS x FROM tn2 WHERE coalesce(n, 15::numeric) <> 15",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "SELECT CASE WHEN k = 'abc' THEN 1 END AS x FROM t WHERE k = 'abc'::varchar",
        &[("x", false)],
    );
}

#[test]
fn an_integer_constant_rounds_as_a_float() {
    // 16777217 as a real is 16777216.
    let db = catalog("CREATE TABLE tf (f real, g int, d double precision);");
    for sql in [
        "SELECT f AS x FROM tf WHERE coalesce(f, 16777217) = 16777216",
        "SELECT CASE WHEN coalesce(f, 16777217) <> 16777216 THEN 1 ELSE f END AS x FROM tf",
        "SELECT f AS x FROM tf WHERE greatest(f, 16777217) = 16777216",
        "SELECT f AS x FROM tf WHERE coalesce(f, 16777217) IS DISTINCT FROM 16777217",
        "SELECT f AS x FROM tf WHERE coalesce(f, 16777217)::int = 16777216",
        "SELECT d AS x FROM tf WHERE coalesce(d, 9007199254740993) = 9007199254740992",
    ] {
        assert_nullable(&db, sql, &[("x", true)]);
    }
    // A float holds small integers exactly.
    for sql in [
        "SELECT f AS x FROM tf WHERE coalesce(f, 0) > 0",
        "SELECT f AS x FROM tf WHERE greatest(f, 5) <> 5",
        "SELECT d AS x FROM tf WHERE coalesce(d, -16777216) <> -16777216",
    ] {
        assert_nullable(&db, sql, &[("x", false)]);
    }
    // Integers stay exact.
    assert_nullable(
        &db,
        "SELECT g AS x FROM tf WHERE coalesce(g, 16777217) = 16777216",
        &[("x", false)],
    );
    // A NULL still folds: a real column NULL makes `coalesce(f, NULL)` NULL.
    assert_nullable(
        &db,
        "SELECT f AS x FROM tf WHERE coalesce(f, NULL) IS NOT NULL",
        &[("x", false)],
    );
}

#[test]
fn a_partition_bound_is_the_value_the_key_stores() {
    let db = catalog(
        "CREATE TABLE q (id int, k text NOT NULL, v int) PARTITION BY LIST (k);
         CREATE TABLE q1 PARTITION OF q FOR VALUES IN ('a'::varchar(1));
         CREATE TABLE q2 PARTITION OF q FOR VALUES IN ('bc'::char(1));
         CREATE TABLE q3 PARTITION OF q FOR VALUES IN ('abc'::varchar(2));
         CREATE TABLE q4 PARTITION OF q FOR VALUES IN ('d   '::char(4));
         CREATE TABLE q5 PARTITION OF q FOR VALUES IN ('e'::varchar);
         CREATE TABLE r (id int, k varchar(2) NOT NULL, v int) PARTITION BY LIST (k);
         CREATE TABLE r1 PARTITION OF r FOR VALUES IN ('xy   ');
         CREATE TABLE n (id int, k int NOT NULL, v int) PARTITION BY LIST (k);
         CREATE TABLE n1 PARTITION OF n FOR VALUES IN (15::numeric(1,-1));
         CREATE TABLE n2 PARTITION OF n FOR VALUES IN (7::numeric);",
    );
    for sql in [
        "SELECT CASE WHEN k = 'bc' THEN 1 END AS x FROM q2",
        "SELECT CASE WHEN k = 'abc' THEN 1 END AS x FROM q3",
        "SELECT CASE WHEN k = 'd   ' THEN 1 END AS x FROM q4",
        "SELECT CASE WHEN k = 15 THEN 1 END AS x FROM n1",
    ] {
        assert_nullable(&db, sql, &[("x", true)]);
    }
    // 'bc'::char(1) is 'b', 'abc'::varchar(2) 'ab', 'd   '::char(4) 'd'
    // once a text key; 15::numeric(1,-1) is 20; a varchar(2) key drops
    // the blanks past its length.
    for sql in [
        "SELECT CASE WHEN k = 'e' THEN 1 END AS x FROM q5",
        "SELECT CASE WHEN k = 'xy' THEN 1 END AS x FROM r1",
        "SELECT CASE WHEN k = 7 THEN 1 END AS x FROM n2",
    ] {
        assert_nullable(&db, sql, &[("x", false)]);
    }
}

// ── Stored expressions follow renames ───────────────────────────────────────

#[test]
fn a_renamed_column_keeps_its_constraints_and_generation() {
    let db = catalog(
        "CREATE TABLE t (id int PRIMARY KEY, a int, b int, CHECK (a IS NOT NULL));
         ALTER TABLE t RENAME COLUMN a TO x;
         ALTER TABLE t RENAME COLUMN b TO a;
         CREATE TABLE t2 (id int PRIMARY KEY, kind text, a_id int, b_id int,
             CHECK (kind = 'a' AND t2.a_id IS NOT NULL OR kind = 'b' AND b_id IS NOT NULL));
         ALTER TABLE t2 RENAME COLUMN a_id TO tmp;
         ALTER TABLE t2 RENAME COLUMN b_id TO a_id;
         ALTER TABLE t2 RENAME COLUMN tmp TO b_id;
         CREATE TABLE g (id int, a int, b int NOT NULL,
             c int GENERATED ALWAYS AS (a + 1) STORED,
             v int GENERATED ALWAYS AS (a + 1) VIRTUAL);
         ALTER TABLE g RENAME COLUMN a TO x;
         ALTER TABLE g RENAME COLUMN b TO a;
         CREATE TABLE g2 (id int, a int NOT NULL, c int GENERATED ALWAYS AS (a * 2) STORED);
         ALTER TABLE g2 RENAME COLUMN a TO z;
         CREATE TABLE p (id int, k int, a int, b int) PARTITION BY RANGE ((a + 0));
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (100);
         ALTER TABLE p RENAME COLUMN a TO x;
         ALTER TABLE p RENAME COLUMN b TO a;
         CREATE TABLE pc (id int, a int, b int) PARTITION BY LIST (a);
         CREATE TABLE pc1 PARTITION OF pc FOR VALUES IN (1);
         ALTER TABLE pc RENAME COLUMN a TO x;
         ALTER TABLE pc RENAME COLUMN b TO a;",
    );
    let mut db = db;
    db.apply_sql("ALTER TABLE pc DETACH PARTITION pc1 CONCURRENTLY;")
        .unwrap();
    assert_nullable(&db, "SELECT a, x FROM t", &[("a", true), ("x", false)]);
    assert_nullable(
        &db,
        "SELECT a_id, b_id FROM t2 WHERE kind = 'a'",
        &[("a_id", true), ("b_id", false)],
    );
    assert_nullable(&db, "SELECT c, v FROM g", &[("c", true), ("v", true)]);
    assert_nullable(&db, "SELECT c FROM g2", &[("c", false)]);
    assert_nullable(&db, "SELECT a, x FROM p1", &[("a", true), ("x", false)]);
    // The detached partition's CHECK is over the key column's new name.
    assert_nullable(&db, "SELECT a, x FROM pc1", &[("a", true), ("x", false)]);
}

#[test]
fn a_renamed_enum_label_renames_it_in_constraints() {
    let db = catalog(
        "CREATE TYPE kind AS ENUM ('a', 'b');
         CREATE TABLE t (id int PRIMARY KEY, k kind NOT NULL CHECK (k = 'a'), v int);
         CREATE TABLE u (id int PRIMARY KEY, k kind NOT NULL, v int,
             CHECK (k <> 'a' OR v IS NOT NULL));
         CREATE TABLE w (id int PRIMARY KEY, k kind NOT NULL, v int,
             CHECK (k <> ALL ('{a}') OR v IS NOT NULL));
         ALTER TYPE kind RENAME VALUE 'a' TO 'z';",
    );
    // The CHECK says `k = 'z'`: never a contradiction for the row.
    assert_nullable(
        &db,
        "SELECT CASE WHEN v > 0 THEN 1 END AS x FROM t",
        &[("x", true)],
    );
    assert_nullable(
        &db,
        "SELECT CASE WHEN k = 'z' THEN 1 END AS x FROM t",
        &[("x", false)],
    );
    assert_nullable(&db, "SELECT v FROM u WHERE k = 'z'", &[("v", false)]);
    assert_nullable(&db, "SELECT v FROM u WHERE k = 'b'", &[("v", true)]);
    // An array of labels isn't rewritten: the CHECK's comparisons go.
    assert_nullable(&db, "SELECT v FROM w WHERE k = 'b'", &[("v", true)]);
    assert_nullable(&db, "SELECT v FROM w WHERE k = 'z'", &[("v", true)]);
}

#[test]
fn constraints_no_row_satisfies_prove_no_branch_unreachable() {
    let db = catalog(
        "CREATE TABLE e (id int PRIMARY KEY, k text NOT NULL CHECK (k = 'a') CHECK (k = 'b'),
             v int);",
    );
    assert_nullable(
        &db,
        "SELECT CASE WHEN v > 0 THEN 1 END AS x FROM e",
        &[("x", true)],
    );
}

// ── Foreign keys and DISABLE TRIGGER ALL across partitions ──────────────────

#[test]
fn disabled_triggers_of_a_partition_stop_its_foreign_keys() {
    let schema = "CREATE TABLE p (a int, b int, PRIMARY KEY (a, b));
         CREATE TABLE c (id int, k int, a int, b int,
             FOREIGN KEY (a, b) REFERENCES p MATCH FULL) PARTITION BY LIST (k);
         CREATE TABLE c1 PARTITION OF c FOR VALUES IN (1);
         CREATE TABLE c2 PARTITION OF c FOR VALUES IN (2);
         CREATE TABLE pp (id int PRIMARY KEY, v int NOT NULL) PARTITION BY RANGE (id);
         CREATE TABLE pp1 PARTITION OF pp FOR VALUES FROM (0) TO (100);
         CREATE TABLE ch (id int PRIMARY KEY, pid int NOT NULL REFERENCES pp);
         CREATE TABLE cc (id int, k int, pid int NOT NULL REFERENCES pp) PARTITION BY LIST (k);
         CREATE TABLE cc1 PARTITION OF cc FOR VALUES IN (1);
         CREATE TABLE cc2 PARTITION OF cc FOR VALUES IN (2);";
    let matched = [
        "SELECT b AS x FROM c WHERE a IS NOT NULL",
        "SELECT p.a AS x FROM c LEFT JOIN p ON p.a = c.a AND p.b = c.b
         WHERE c.a IS NOT NULL AND c.b IS NOT NULL",
        "SELECT b AS x FROM c1 WHERE a IS NOT NULL",
    ];
    let referenced = "SELECT pp.v AS x FROM ch LEFT JOIN pp ON pp.id = ch.pid";
    let partitioned_child = "SELECT pp.v AS x FROM cc LEFT JOIN pp ON pp.id = cc.pid";
    let fresh = catalog(schema);
    for sql in matched.iter().chain([&referenced, &partitioned_child]) {
        assert_nullable(&fresh, sql, &[("x", false)]);
    }
    // A partition of the referencing table losing its triggers takes in
    // rows without their referenced row.
    let mut db = catalog(schema);
    db.apply_sql("ALTER TABLE cc2 DISABLE TRIGGER ALL;")
        .unwrap();
    assert_nullable(&db, partitioned_child, &[("x", true)]);
    assert_nullable(&db, referenced, &[("x", false)]);
    for disable in [
        "ALTER TABLE c1 DISABLE TRIGGER ALL;",
        "ALTER TABLE c DISABLE TRIGGER ALL;",
    ] {
        let mut db = catalog(schema);
        db.apply_sql(disable).unwrap();
        for sql in matched {
            assert_nullable(&db, sql, &[("x", true)]);
        }
        db.apply_sql(&disable.replace("DISABLE", "ENABLE")).unwrap();
        for sql in matched {
            assert_nullable(&db, sql, &[("x", false)]);
        }
    }
    // A partition of the referenced table losing its triggers loses the
    // referenced rows' protection.
    let mut db = catalog(schema);
    db.apply_sql("ALTER TABLE pp1 DISABLE TRIGGER ALL;")
        .unwrap();
    assert_nullable(&db, referenced, &[("x", true)]);
    assert_nullable(&db, partitioned_child, &[("x", true)]);
    // Another table's triggers don't matter.
    let mut db = catalog(schema);
    db.apply_sql("ALTER TABLE c2 DISABLE TRIGGER ALL;").unwrap();
    assert_nullable(&db, referenced, &[("x", false)]);
}

// ── Rows that aren't there ──────────────────────────────────────────────────

#[test]
fn an_aggregate_row_from_no_input_reaches_every_branch() {
    let db = catalog(
        "CREATE TABLE t (id int PRIMARY KEY, kind text NOT NULL CHECK (kind IN ('a', 'b')), v int);
         CREATE TYPE e AS ENUM ('x', 'y');
         CREATE TABLE u (id int PRIMARY KEY, k e NOT NULL);",
    );
    for sql in [
        "SELECT CASE WHEN count(*) > 5 THEN 1 END AS x FROM t WHERE kind = 'c'",
        "SELECT CASE WHEN count(*) > 5 THEN 1 END AS x FROM t WHERE id IS NULL",
        "SELECT CASE WHEN count(*) > 5 THEN 1 END AS x FROM u WHERE k <> 'x' AND k <> 'y'",
        "SELECT CASE WHEN count(*) > 5 THEN 1 END AS x FROM t WHERE kind = 'c' GROUP BY ()",
        "SELECT CASE WHEN count(*) > 5 THEN 1 END AS x FROM t JOIN u ON t.kind = 'c'",
        "SELECT (SELECT CASE WHEN count(*) > 5 THEN 1 END FROM t WHERE kind = 'c') AS x",
        "SELECT x FROM (SELECT CASE WHEN count(*) > 5 THEN 1 END AS x FROM t
                        WHERE kind NOT IN ('a', 'b')) s",
        "SELECT (SELECT CASE WHEN count(*) > 5 THEN 1 END FROM u WHERE t.kind = 'c') AS x FROM t",
    ] {
        assert_nullable(&db, sql, &[("x", true)]);
    }
    // Groups are made of rows, and an aggregate's arguments read rows.
    assert_nullable(
        &db,
        "SELECT CASE WHEN kind = 'a' THEN 1 WHEN kind = 'b' THEN 2 END AS x FROM t GROUP BY kind",
        &[("x", false)],
    );
    assert_eq!(
        elements(
            &db,
            "SELECT array_agg(CASE WHEN kind = 'a' THEN 1 WHEN kind = 'b' THEN 2 END) AS x
             FROM t WHERE kind <> 'c'"
        ),
        Some(false)
    );
    assert_nullable(
        &db,
        "SELECT CASE WHEN k = 'x' THEN 1 WHEN k = 'y' THEN 2 END AS x FROM u",
        &[("x", false)],
    );
}

#[test]
fn a_where_fact_on_an_expression_misses_the_empty_grouping_set() {
    let db = catalog(
        "CREATE TABLE t (id int PRIMARY KEY, a int, j jsonb);
         CREATE TABLE s (id int, v int);",
    );
    for sql in [
        "SELECT (SELECT max(v) FROM s) AS x FROM t
         WHERE (SELECT max(v) FROM s) IS NOT NULL GROUP BY ()",
        "SELECT (SELECT max(v) FROM s) AS x FROM t
         WHERE (SELECT max(v) FROM s) IS NOT NULL GROUP BY GROUPING SETS ((), ())",
        "SELECT (SELECT max(v) FROM s) AS x FROM t
         WHERE (SELECT max(v) FROM s) IS NOT NULL HAVING true",
        "SELECT (SELECT o.j ->> 'z' FROM s WHERE o.j ->> 'z' IS NOT NULL GROUP BY ()) AS x
         FROM t o",
    ] {
        assert_nullable(&db, sql, &[("x", true)]);
    }
    // An aggregate reads rows past WHERE.
    assert_eq!(
        elements(
            &db,
            "SELECT array_agg(j ->> 'k') AS x FROM t WHERE j ->> 'k' IS NOT NULL"
        ),
        Some(false)
    );
    // Groups of rows past WHERE: it holds.
    assert_nullable(
        &db,
        "SELECT (SELECT max(v) FROM s) AS x FROM t
         WHERE (SELECT max(v) FROM s) IS NOT NULL GROUP BY t.id",
        &[("x", false)],
    );
}

#[test]
fn an_outer_aggregate_in_a_subquery_reads_the_outer_rows() {
    let db = catalog("CREATE TABLE t (id int, h int NOT NULL);");
    for sql in [
        "SELECT (SELECT max(t.h)) AS x FROM t",
        "SELECT (SELECT max(t.h) WHERE true) AS x FROM t",
        "SELECT (SELECT max(t.h) FROM (VALUES (1)) v (y)) AS x FROM t",
        "SELECT (SELECT JSON_ARRAYAGG(t.h)) AS x FROM t",
    ] {
        assert_nullable(&db, sql, &[("x", true)]);
    }
    assert_nullable(
        &db,
        "SELECT (SELECT count(t.h)) AS x FROM t",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "SELECT (SELECT max(t.h)) AS x FROM t GROUP BY id",
        &[("x", true)],
    );
    assert_nullable(
        &db,
        "SELECT max(h) AS x FROM t GROUP BY id",
        &[("x", false)],
    );
}

// ── Grouping sets ───────────────────────────────────────────────────────────

#[test]
fn a_column_a_grouping_set_nulls_out_says_nothing_of_its_row() {
    let db = catalog(
        "CREATE TABLE t (g int, h int, CHECK (g IS NOT NULL OR h IS NOT NULL));
         CREATE TABLE t2 (g int, h int, CHECK (g <> 1 OR h IS NOT NULL));
         CREATE TABLE a (id int, v int NOT NULL);",
    );
    for sql in [
        "SELECT CASE WHEN g IS NULL THEN h END AS x FROM t GROUP BY GROUPING SETS ((g, h), (h))",
        "SELECT h AS x FROM t GROUP BY GROUPING SETS ((g, h), (h)) HAVING g IS NULL",
        "SELECT CASE WHEN g IS NOT NULL THEN 0 ELSE h END AS x FROM t
         GROUP BY GROUPING SETS ((g, h), (h))",
        "SELECT CASE WHEN g <> 1 THEN 0 ELSE h END AS x FROM t2 WHERE g IS NOT NULL
         GROUP BY GROUPING SETS ((g, h), (h))",
        "SELECT a.v AS x FROM a GROUP BY GROUPING SETS ((a.id, a.v), (a.id))
         HAVING a.id IS NOT NULL",
        "SELECT CASE WHEN a.id IS NOT NULL THEN a.v END AS x FROM a
         GROUP BY GROUPING SETS ((a.id, a.v), (a.id))",
        "SELECT CASE WHEN s.id IS NOT NULL THEN s.v ELSE 0 END AS x FROM (SELECT * FROM a) s
         GROUP BY GROUPING SETS ((s.id, s.v), (s.id))",
    ] {
        assert_nullable(&db, sql, &[("x", true)]);
    }
    assert_eq!(
        elements(
            &db,
            "SELECT array_agg(h) AS x FROM t GROUP BY GROUPING SETS ((g), ()) HAVING g IS NULL"
        ),
        Some(true)
    );
    // What WHERE proves of the rows still holds of them.
    assert_nullable(
        &db,
        "SELECT h AS x FROM t WHERE g IS NULL GROUP BY GROUPING SETS ((g, h), (h))",
        &[("x", false)],
    );
    // A column read non-NULL is its row's value.
    assert_nullable(
        &db,
        "SELECT CASE WHEN g = 1 THEN h ELSE 0 END AS x FROM t2 GROUP BY GROUPING SETS ((g, h), (h))",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "SELECT a.v AS x FROM a GROUP BY GROUPING SETS ((a.id, a.v), (a.id))
         HAVING a.v IS NOT NULL",
        &[("x", false)],
    );
}

// ── Window calls read other rows ────────────────────────────────────────────

#[test]
fn a_window_call_reads_rows_a_branch_knows_nothing_of() {
    let db = catalog("CREATE TABLE t (id int PRIMARY KEY, x int, g int);");
    for sql in [
        "SELECT CASE WHEN x IS NOT NULL THEN first_value(x) OVER (ORDER BY id) END AS a FROM t",
        "SELECT CASE WHEN x IS NOT NULL THEN nth_value(x, 1) OVER (ORDER BY id) ELSE 0 END AS a
         FROM t",
        "SELECT CASE WHEN x IS NOT NULL THEN min(x) OVER (ORDER BY id DESC) ELSE 0 END AS a FROM t",
        "SELECT x IS NULL OR first_value(x) OVER (ORDER BY id) > 0 AS a FROM t",
        "SELECT x IS NOT NULL AND (array_agg(x) OVER ())[1] > 0 AS a FROM t",
    ] {
        assert_nullable(&db, sql, &[("a", true)]);
    }
    assert_eq!(
        elements(
            &db,
            "SELECT CASE WHEN x IS NOT NULL THEN array_agg(x) OVER () ELSE '{}' END AS a FROM t"
        ),
        Some(true)
    );
    // What WHERE or HAVING proves holds of every row.
    assert_nullable(
        &db,
        "SELECT CASE WHEN x IS NOT NULL THEN first_value(x) OVER (ORDER BY id) ELSE 0 END AS a
         FROM t WHERE x IS NOT NULL",
        &[("a", false)],
    );
    assert_nullable(
        &db,
        "SELECT first_value(g) OVER () AS a FROM t GROUP BY g HAVING g IS NOT NULL",
        &[("a", false)],
    );
    // An aggregate's rows share a grouped column.
    assert_eq!(
        elements(
            &db,
            "SELECT CASE WHEN x IS NOT NULL THEN array_agg(x) ELSE '{}' END AS a FROM t GROUP BY x"
        ),
        Some(false)
    );
}

// ── Expression facts: the same value each time ──────────────────────────────

#[test]
fn between_runs_the_ordering_operators() {
    let volatile = catalog(
        "CREATE TABLE t (x int, y text);
         CREATE FUNCTION vge(int, text) RETURNS bool LANGUAGE plpgsql VOLATILE AS
             $$ BEGIN IF random() < 0.5 THEN RETURN NULL; END IF; RETURN true; END $$;
         CREATE OPERATOR >= (LEFTARG = int, RIGHTARG = text, FUNCTION = vge);
         CREATE OPERATOR <= (LEFTARG = int, RIGHTARG = text, FUNCTION = vge);",
    );
    let sql = "SELECT (x BETWEEN y AND y) AS b FROM t WHERE (x BETWEEN y AND y) IS NOT NULL";
    assert_nullable(&volatile, sql, &[("b", true)]);
    let plain = catalog("CREATE TABLE t (x int, y int);");
    assert_nullable(
        &plain,
        "SELECT (x BETWEEN y AND y) AS b FROM t WHERE (x BETWEEN y AND y) IS NOT NULL",
        &[("b", false)],
    );
}

#[test]
fn an_implicit_volatile_cast_runs_each_time() {
    let schema = "CREATE TABLE t (x int);
         CREATE TYPE c AS (a int);
         CREATE FUNCTION fc(c) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT ($1).a';";
    let sql = "SELECT fc(x::c) AS r FROM t WHERE fc(x::c) IS NOT NULL";
    let mut db = catalog(schema);
    db.apply_sql(
        "CREATE FUNCTION mk(int) RETURNS c LANGUAGE plpgsql IMMUTABLE AS
             $$ BEGIN RETURN ROW($1)::c; END $$;
         CREATE CAST (int AS c) WITH FUNCTION mk(int);",
    )
    .unwrap();
    assert_nullable(&db, sql, &[("r", false)]);
    let mut db = catalog(schema);
    db.apply_sql(
        "CREATE FUNCTION mk(int) RETURNS c LANGUAGE plpgsql VOLATILE AS
             $$ BEGIN IF random() < 0.5 THEN RETURN ROW(NULL)::c; END IF; RETURN ROW($1)::c; END $$;
         CREATE CAST (int AS c) WITH FUNCTION mk(int) AS IMPLICIT;",
    )
    .unwrap();
    assert_nullable(&db, sql, &[("r", true)]);
    assert_nullable(
        &db,
        "SELECT fc(x) AS r FROM t WHERE fc(x) IS NOT NULL",
        &[("r", true)],
    );
}

#[test]
fn a_nested_cte_does_not_hide_a_volatile_view() {
    let db = catalog(
        "CREATE SEQUENCE seq;
         CREATE TABLE seed (id int PRIMARY KEY);
         CREATE TABLE st (id int PRIMARY KEY, x int);
         CREATE VIEW v AS SELECT id,
             CASE WHEN nextval('seq'::regclass) % 2 = 1 THEN 7 END AS x FROM seed;",
    );
    assert_nullable(
        &db,
        "SELECT (SELECT v.x + (WITH v AS (SELECT 0) SELECT 0) FROM v WHERE v.id = s.id) AS r
         FROM seed s
         WHERE (SELECT v.x + (WITH v AS (SELECT 0) SELECT 0) FROM v WHERE v.id = s.id) IS NOT NULL",
        &[("r", true)],
    );
    // A table under the same name as a nested CTE is stable.
    assert_nullable(
        &db,
        "SELECT (SELECT st.x + (WITH st AS (SELECT 0) SELECT 0) FROM st WHERE st.id = s.id) AS r
         FROM seed s
         WHERE (SELECT st.x + (WITH st AS (SELECT 0) SELECT 0) FROM st WHERE st.id = s.id)
             IS NOT NULL",
        &[("r", false)],
    );
}

#[test]
fn a_call_relying_on_a_default_runs_the_default() {
    let db = catalog(
        "CREATE SEQUENCE seq;
         CREATE FUNCTION f(n bigint DEFAULT nextval('seq'::regclass)) RETURNS int
             LANGUAGE sql IMMUTABLE AS $$ SELECT CASE WHEN n % 2 = 1 THEN 7 END $$;
         CREATE FUNCTION g(n bigint DEFAULT 5) RETURNS int
             LANGUAGE sql IMMUTABLE AS $$ SELECT CASE WHEN n % 2 = 1 THEN 7 END $$;
         CREATE TABLE one (id int PRIMARY KEY);",
    );
    // A constant default gives the same value each time.
    assert_nullable(
        &db,
        "SELECT g() AS r FROM one WHERE g() IS NOT NULL",
        &[("r", false)],
    );
    assert_nullable(
        &db,
        "SELECT f() AS r FROM one WHERE f() IS NOT NULL",
        &[("r", true)],
    );
    assert_nullable(
        &db,
        "SELECT f(id) AS r FROM one WHERE f(id) IS NOT NULL",
        &[("r", false)],
    );
}

// ── A NULL a domain rejects ─────────────────────────────────────────────────

#[test]
fn a_null_a_domain_rejects_fails_every_execution() {
    let db = catalog(
        "CREATE DOMAIN nn AS int CHECK (VALUE IS NOT NULL);
         CREATE DOMAIN nn2 AS int NOT NULL;
         CREATE DOMAIN nn3 AS int CHECK (VALUE > 0);
         CREATE DOMAIN outer2 AS nn2;
         CREATE DOMAIN dflt AS int NOT NULL DEFAULT 5;
         CREATE DOMAIN dflt2 AS dflt;
         CREATE TABLE t (id int, d nn, e nn2);
         CREATE TABLE t2 (id int, d nn);
         CREATE TABLE t3 (id int, f nn3);
         CREATE TABLE t4 (id int, x outer2);
         CREATE TABLE t5 (id int, x dflt2);
         CREATE TABLE t6 (id int, x dflt DEFAULT NULL);",
    );
    for (sql, msg) in [
        (
            "SELECT NULL::nn2 AS x",
            "domain nn2 does not allow null values",
        ),
        (
            "SELECT NULL::outer2 AS x",
            "domain outer2 does not allow null values",
        ),
        (
            "SELECT CAST(NULL AS nn) AS x",
            "value for domain nn violates check constraint \"nn_check\"",
        ),
        (
            "VALUES (1), (NULL::nn2)",
            "domain nn2 does not allow null values",
        ),
        (
            "SELECT 1 UNION ALL SELECT NULL::nn2",
            "domain nn2 does not allow null values",
        ),
        (
            "INSERT INTO t (id, d) VALUES (1, 1)",
            "domain nn2 does not allow null values",
        ),
        (
            "INSERT INTO t2 (id) VALUES (1)",
            "value for domain nn violates check constraint \"nn_check\"",
        ),
        (
            "INSERT INTO t2 (id, d) VALUES (1, DEFAULT)",
            "value for domain nn violates check constraint \"nn_check\"",
        ),
        (
            "INSERT INTO t2 DEFAULT VALUES",
            "value for domain nn violates check constraint \"nn_check\"",
        ),
        (
            "INSERT INTO t2 (id, d) VALUES (1, NULL)",
            "value for domain nn violates check constraint \"nn_check\"",
        ),
        (
            "INSERT INTO t4 (id) VALUES (1)",
            "domain outer2 does not allow null values",
        ),
        (
            "INSERT INTO t4 (id, x) VALUES (1, NULL)",
            "domain outer2 does not allow null values",
        ),
        (
            "INSERT INTO t6 (id) VALUES (1)",
            "domain dflt does not allow null values",
        ),
    ] {
        let err = db.analyze(sql).expect_err(sql).to_string();
        assert!(err.starts_with(msg), "`{sql}`: {err}");
    }
    // Never evaluated, or not rejected.
    for sql in [
        "SELECT CAST(NULL AS nn3) AS x",
        "SELECT NULL::nn2 AS x FROM t",
        "SELECT NULL::nn2 AS x LIMIT 0",
        "INSERT INTO t3 (id) VALUES (1)",
        "INSERT INTO t2 (id) SELECT 1 WHERE false",
        "INSERT INTO t5 (id) VALUES (1)",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
}
