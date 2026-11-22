//! Soundness regressions: shapes where the analyzer used to infer NOT NULL
//! for a value PostgreSQL 18 returns NULL for, each found by the
//! nullability gap hunt and reproduced on PG 18 with data satisfying the
//! schema.

use crate::common::*;

#[track_caller]
fn nullable(db: &PgCatalog, sql: &str) -> Vec<bool> {
    db.analyze(sql)
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .columns
        .iter()
        .map(|c| c.nullable)
        .collect()
}

const PARENT_CHILD: &str = "CREATE TABLE a (id int PRIMARY KEY, v int NOT NULL);
     CREATE TABLE b (id int PRIMARY KEY, a_id int NOT NULL REFERENCES a, v int NOT NULL);";

#[test]
fn foreign_keys_are_not_inherited_by_inheritance_children() {
    // `INSERT INTO bk VALUES (1, 999, 1)` is fine: bk has no foreign key,
    // and a scan of b returns it.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(&format!("{PARENT_CHILD} CREATE TABLE bk () INHERITS (b);"))
        .unwrap();
    let join = "LEFT JOIN a ON a.id = b.a_id";
    assert_eq!(nullable(&db, &format!("SELECT a.v FROM b {join}")), [true]);
    assert_eq!(
        nullable(&db, &format!("SELECT a.v FROM ONLY b {join}")),
        [false]
    );
}

#[test]
fn a_foreign_key_into_a_partitioned_table_is_not_one_into_a_partition() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE pa (id int PRIMARY KEY, v int NOT NULL) PARTITION BY RANGE (id);
         CREATE TABLE pa1 PARTITION OF pa FOR VALUES FROM (0) TO (100);
         CREATE TABLE pa2 PARTITION OF pa FOR VALUES FROM (100) TO (200);
         CREATE TABLE pb (id int PRIMARY KEY, a_id int NOT NULL REFERENCES pa);",
    )
    .unwrap();
    assert_eq!(
        nullable(&db, "SELECT a.v FROM pb LEFT JOIN pa1 a ON a.id = pb.a_id"),
        [true]
    );
    assert_eq!(
        nullable(&db, "SELECT a.v FROM pb LEFT JOIN pa a ON a.id = pb.a_id"),
        [false]
    );
}

#[test]
fn row_locking_rechecks_rows_but_not_foreign_keys() {
    // Under READ COMMITTED a locked row updated concurrently is re-fetched
    // and re-joined with the parent row read before (EvalPlanQual): its
    // new key may reference another parent.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(PARENT_CHILD).unwrap();
    for lock in [
        "FOR UPDATE OF b",
        "FOR SHARE OF b",
        "FOR NO KEY UPDATE OF b",
    ] {
        assert_eq!(
            nullable(
                &db,
                &format!("SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id {lock}")
            ),
            [true],
            "{lock}"
        );
    }
    assert_eq!(
        nullable(
            &db,
            "SELECT s.v FROM (SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id FOR UPDATE OF b) s"
        ),
        [true]
    );
}

#[test]
fn disabled_triggers_stop_enforcing_foreign_keys() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(&format!(
        "{PARENT_CHILD} ALTER TABLE b DISABLE TRIGGER ALL;"
    ))
    .unwrap();
    let q = "SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id";
    assert_eq!(nullable(&db, q), [true]);
    // Enabling them again checks none of the rows written meanwhile.
    db.apply_sql("ALTER TABLE b ENABLE TRIGGER ALL;").unwrap();
    assert_eq!(nullable(&db, q), [true]);
    // The parent's triggers enforce its side (a referenced row can't go).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(PARENT_CHILD).unwrap();
    assert_eq!(nullable(&db, q), [false]);
    db.apply_sql("ALTER TABLE a DISABLE TRIGGER ALL;").unwrap();
    assert_eq!(nullable(&db, q), [true]);
}

#[test]
fn dropping_the_referenced_key_with_cascade_drops_the_foreign_key() {
    let q = "SELECT p.id FROM chi c LEFT JOIN par p ON p.id = c.pid";
    for (schema, drop) in [
        (
            "CREATE TABLE par (id int PRIMARY KEY);",
            "ALTER TABLE par DROP CONSTRAINT par_pkey CASCADE;",
        ),
        (
            "CREATE TABLE par (id int, CONSTRAINT u UNIQUE (id));",
            "ALTER TABLE par DROP CONSTRAINT u CASCADE;",
        ),
        (
            "CREATE TABLE par (id int NOT NULL); CREATE UNIQUE INDEX ui ON par (id);",
            "DROP INDEX ui CASCADE;",
        ),
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(&format!(
            "{schema} CREATE TABLE chi (id int PRIMARY KEY, pid int NOT NULL CONSTRAINT fk REFERENCES par (id));"
        ))
        .unwrap();
        assert_eq!(nullable(&db, q), [false], "{schema}");
        db.apply_sql(drop).unwrap();
        assert_eq!(nullable(&db, q), [true], "{drop}");
        // The foreign key is gone.
        assert!(db.apply_sql("ALTER TABLE chi DROP CONSTRAINT fk;").is_err());
    }
    // Without CASCADE the index can't go.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE par (id int NOT NULL); CREATE UNIQUE INDEX ui ON par (id);
         CREATE TABLE chi (id int PRIMARY KEY, pid int NOT NULL CONSTRAINT fk REFERENCES par (id));",
    )
    .unwrap();
    let err = db.apply_sql("DROP INDEX ui;").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot drop index ui because other objects depend on it"),
        "{err}"
    );
}

#[test]
fn views_follow_constraints_relaxed_after_them() {
    for relax in [
        "ALTER TABLE chi DROP CONSTRAINT fk;",
        "ALTER TABLE chi ALTER CONSTRAINT fk NOT ENFORCED;",
        "ALTER TABLE par ENABLE ROW LEVEL SECURITY;",
        "ALTER TABLE chi DISABLE TRIGGER ALL;",
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE par (id int PRIMARY KEY);
             CREATE TABLE chi (id int PRIMARY KEY, pid int NOT NULL CONSTRAINT fk REFERENCES par);
             CREATE VIEW vf AS SELECT c.id, p.id AS pid FROM chi c LEFT JOIN par p ON p.id = c.pid;
             CREATE MATERIALIZED VIEW mf AS
                 SELECT c.id, p.id AS pid FROM chi c LEFT JOIN par p ON p.id = c.pid;",
        )
        .unwrap();
        assert_eq!(nullable(&db, "SELECT pid FROM vf"), [false]);
        db.apply_sql(relax).unwrap();
        assert_eq!(nullable(&db, "SELECT pid FROM vf"), [true], "{relax}");
        assert_eq!(nullable(&db, "SELECT pid FROM mf"), [true], "{relax}");
    }
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE c6 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CONSTRAINT ck CHECK (kind <> 'a' OR a_id IS NOT NULL));
         CREATE VIEW va AS SELECT id, a_id FROM c6 WHERE kind = 'a';",
    )
    .unwrap();
    assert_eq!(nullable(&db, "SELECT a_id FROM va"), [false]);
    db.apply_sql("ALTER TABLE c6 DROP CONSTRAINT ck;").unwrap();
    assert_eq!(nullable(&db, "SELECT a_id FROM va"), [true]);
}

#[test]
fn a_parent_s_not_null_must_hold_in_its_children() {
    for schema in [
        "CREATE TABLE ip (id int, a int NOT NULL NO INHERIT); CREATE TABLE ic () INHERITS (ip);",
        "CREATE TABLE ip (id int, a int NOT NULL NO INHERIT); CREATE TABLE ic (id int, a int);
         ALTER TABLE ic INHERIT ip;",
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(schema).unwrap();
        assert_eq!(nullable(&db, "SELECT a FROM ip"), [true], "{schema}");
        assert_eq!(nullable(&db, "SELECT a FROM ONLY ip"), [false], "{schema}");
        assert_eq!(
            nullable(&db, "DELETE FROM ip WHERE id = 1 RETURNING a"),
            [true],
            "{schema}"
        );
    }
}

#[test]
fn a_single_out_column_function_is_scalar() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, j jsonb NOT NULL);
         CREATE FUNCTION f() RETURNS TABLE (x int) LANGUAGE sql AS 'SELECT NULL::int';
         CREATE FUNCTION g(OUT v int) LANGUAGE sql AS 'SELECT NULL::int';",
    )
    .unwrap();
    for sql in [
        "SELECT e FROM t, jsonb_array_elements_text(t.j) e",
        "SELECT e FROM t CROSS JOIN LATERAL json_array_elements_text(t.j::json) e",
        "SELECT e FROM f() e",
        "SELECT e FROM g() e",
    ] {
        let s = db.analyze(sql).unwrap();
        assert!(s.columns[0].nullable, "{sql}");
        assert!(
            !matches!(s.columns[0].pg_type, Type::AnonymousRecord { .. }),
            "{sql}: {:?}",
            s.columns[0].pg_type
        );
    }
    // Several functions or WITH ORDINALITY: a record.
    let s = db
        .analyze("SELECT e FROM jsonb_array_elements_text('[1]') WITH ORDINALITY e")
        .unwrap();
    assert!(matches!(s.columns[0].pg_type, Type::AnonymousRecord { .. }));
}

#[test]
fn grouped_expressions_a_grouping_set_leaves_out_are_null() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, g int NOT NULL, s text NOT NULL, b int);
         CREATE TABLE t2 (g int NOT NULL);",
    )
    .unwrap();
    for sql in [
        "SELECT g + 1 AS k, count(*) FROM t GROUP BY ROLLUP (g + 1)",
        "SELECT t.g + 1 AS k, count(*) FROM t GROUP BY ROLLUP (g + 1)",
        "SELECT lower(s) AS k, count(*) FROM t GROUP BY CUBE (lower(s))",
        "SELECT coalesce(b, 0) AS k, count(*) FROM t GROUP BY ROLLUP (coalesce(b, 0))",
        "SELECT g + 1 AS k, count(*) FROM t GROUP BY GROUPING SETS (1, ())",
        "SELECT g + 1 AS k, count(*) FROM t GROUP BY ROLLUP (k)",
        "SELECT (g + 1) * 2 AS k, count(*) FROM t GROUP BY ROLLUP (g + 1)",
        // A USING merged column and its constituent are one grouped value.
        "SELECT g AS k, count(*) FROM t JOIN t2 USING (g) GROUP BY ROLLUP (t.g)",
        "SELECT t.g AS k, count(*) FROM t JOIN t2 USING (g) GROUP BY ROLLUP (g)",
        "SELECT t2.g AS k, count(*) FROM t RIGHT JOIN t2 USING (g) GROUP BY ROLLUP (g)",
    ] {
        assert_eq!(nullable(&db, sql), [true, false], "{sql}");
    }
    // In every grouping set, it's never nulled out.
    assert_eq!(
        nullable(&db, "SELECT g + 1 AS k, count(*) FROM t GROUP BY g + 1"),
        [false, false]
    );
    assert_eq!(
        nullable(
            &db,
            "SELECT g + 1 AS k, count(*) FROM t GROUP BY GROUPING SETS ((g + 1), (g + 1, s))"
        ),
        [false, false]
    );
}

#[test]
fn unnest_of_a_multidimensional_array_reads_the_inner_elements() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id int PRIMARY KEY, arr int[] NOT NULL, nn int NOT NULL);")
        .unwrap();
    for sql in [
        "SELECT x FROM t, unnest(ARRAY[t.arr, t.arr]) x",
        "SELECT y FROM t, unnest(ARRAY[t.arr]) y",
        "SELECT unnest(ARRAY[t.arr]) AS y FROM t",
        "SELECT x FROM unnest(ARRAY[[1, 2], [3, NULL]]) x",
        "SELECT y FROM unnest(ARRAY[ARRAY[1, NULL]]) y",
    ] {
        assert_eq!(nullable(&db, sql), [true], "{sql}");
    }
    // What the argument says of its elements still counts.
    assert_eq!(
        nullable(&db, "SELECT x FROM unnest(ARRAY[1, 2]) x"),
        [false]
    );
    assert_eq!(
        nullable(&db, "SELECT x FROM unnest(ARRAY(SELECT nn FROM t)) x"),
        [false]
    );
}

#[test]
fn a_comparison_under_another_collation_or_type_proves_no_equality() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE TABLE c6 (id int PRIMARY KEY, kind text NOT NULL, a_id int,
             CHECK (kind <> 'a' OR a_id IS NOT NULL));",
    )
    .unwrap();
    // Under `ci`, `'A' = 'a'`: the row ('A', NULL) satisfies the CHECK.
    assert_eq!(
        nullable(&db, "SELECT a_id FROM c6 WHERE kind = 'a' COLLATE ci"),
        [true]
    );
    assert_eq!(
        nullable(&db, "SELECT a_id FROM c6 WHERE kind = 'a'::varchar"),
        [true]
    );
    assert_eq!(
        nullable(&db, "SELECT a_id FROM c6 WHERE kind = 'a'::text"),
        [false]
    );
    assert_eq!(
        nullable(&db, "SELECT a_id FROM c6 WHERE kind = 'a'"),
        [false]
    );
}

#[test]
fn a_strict_sql_operator_can_still_return_null() {
    // `STRICT` only skips the call on a NULL argument: the body may return
    // NULL for any other input.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL);
         CREATE FUNCTION half(int, int) RETURNS int LANGUAGE sql STRICT IMMUTABLE
             AS 'SELECT NULLIF($1, $2)';
         CREATE OPERATOR %% (LEFTARG = int, RIGHTARG = int, FUNCTION = half);",
    )
    .unwrap();
    assert_eq!(nullable(&db, "SELECT a %% a AS v FROM t"), [true]);
}

#[test]
fn rows_written_while_triggers_were_disabled_stay_unchecked() {
    // Re-enabling the triggers doesn't check the rows written meanwhile:
    // PG 19.0 returns (NULL) for both queries.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(&format!(
        "{PARENT_CHILD}
         ALTER TABLE b DISABLE TRIGGER ALL;
         INSERT INTO b VALUES (1, 999, 1);
         ALTER TABLE b ENABLE TRIGGER ALL;"
    ))
    .unwrap();
    assert_eq!(
        nullable(&db, "SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id"),
        [true]
    );
    assert_eq!(
        nullable(&db, "SELECT (SELECT a.v FROM a WHERE a.id = b.a_id) FROM b"),
        [true]
    );
}

#[test]
fn a_materialized_view_keeps_its_rows_until_refreshed() {
    // A matview stores the rows its query returned at CREATE / REFRESH
    // time: a NOT NULL set on the base table afterwards (once its NULLs
    // were updated away) says nothing of them. On PG 19.0, with a row
    // (1, NULL) in t before the matview is created, `SELECT a FROM mv`
    // returns NULL.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int);
         CREATE MATERIALIZED VIEW mv AS SELECT id, a FROM t;
         UPDATE t SET a = 0 WHERE a IS NULL;
         ALTER TABLE t ALTER COLUMN a SET NOT NULL;",
    )
    .unwrap();
    assert_eq!(nullable(&db, "SELECT a FROM mv"), [true]);
}

#[test]
fn rows_written_under_replica_role_stay_unchecked() {
    // `session_replication_role = replica` fires no RI trigger: PG 19.0
    // returns (NULL) for the orphan row written meanwhile.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(&format!(
        "{PARENT_CHILD}
         SET session_replication_role = replica;
         INSERT INTO b VALUES (1, 999, 1);
         RESET session_replication_role;"
    ))
    .unwrap();
    assert_eq!(
        nullable(&db, "SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id"),
        [true]
    );
    // Any way a migration sets it to `replica`, or to a value it doesn't
    // state. (Not mirrored on the oracle: the setting would outlive the
    // test — on its connection, its role, its cluster's database.)
    for setting in [
        "SET LOCAL session_replication_role = 'replica';",
        "SELECT set_config('session_replication_role', 'replica', false);",
        "SELECT set_config('session_replication_role', v, false) FROM (VALUES ('origin')) s (v);",
        "SELECT set_config(n, 'replica', false) FROM (VALUES ('x.y')) s (n);",
        "ALTER DATABASE appdb SET session_replication_role = replica;",
        "CREATE ROLE app; ALTER ROLE app SET session_replication_role = replica;",
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
         SET session_replication_role = replica AS 'SELECT 1';",
        "CREATE PROCEDURE pr() LANGUAGE sql AS 'SELECT 1'; \
         ALTER PROCEDURE pr() SET session_replication_role = replica;",
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.skip_pg_sanity();
        db.apply_sql(&format!("{PARENT_CHILD} {setting}")).unwrap();
        assert_eq!(
            nullable(&db, "SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id"),
            [true],
            "{setting}"
        );
    }
    // Setting it to `origin` or `local`, RI triggers fire.
    for setting in [
        "SET session_replication_role = origin;",
        "SET session_replication_role = local;",
        "SELECT set_config('session_replication_role', 'origin', false);",
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(&format!("{PARENT_CHILD} {setting}")).unwrap();
        assert_eq!(
            nullable(&db, "SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id"),
            [false],
            "{setting}"
        );
    }
}

/// What a routine's body might do to the checks — dynamic SQL, `DISABLE
/// TRIGGER`, a catalog write — doesn't void a foreign key for nullability
/// (declared constraints are trusted), while the foreign-key join removal,
/// which would return wrong rows, still gives up.
#[test]
fn a_routine_body_that_may_turn_checks_off_leaves_the_foreign_key_trusted() {
    for routine in [
        "CREATE FUNCTION off() RETURNS void LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN EXECUTE 'ALTER TABLE b DISABLE TRIGGER ALL'; END IF; END $$;",
        "DO $$ BEGIN IF false THEN EXECUTE 'SET session_replication_role = replica'; \
         END IF; END $$;",
        "CREATE FUNCTION n() RETURNS bigint LANGUAGE sql \
         BEGIN ATOMIC SELECT count(*) FROM pg_catalog.pg_constraint; END;",
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(&format!("{PARENT_CHILD} {routine}")).unwrap();
        assert_eq!(
            nullable(&db, "SELECT a.v FROM b LEFT JOIN a ON a.id = b.a_id"),
            [false],
            "{routine}"
        );
        let join = db
            .analyze("SELECT b.id FROM b JOIN a ON a.id = b.a_id")
            .unwrap();
        assert!(join.simplifications.is_empty(), "{routine}");
    }
    // Without it, the join is dropped.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(PARENT_CHILD).unwrap();
    let join = db
        .analyze("SELECT b.id FROM b JOIN a ON a.id = b.a_id")
        .unwrap();
    assert!(!join.simplifications.is_empty());
}

#[test]
fn refreshing_a_materialized_view_stores_rows_of_its_query_as_it_stands() {
    // REFRESH replaces the rows: on PG 19.0 `SELECT a FROM mv` returns 0
    // after it. A relaxed base column reaches the matview at once (the
    // application may refresh it at any time), and views over it follow.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int);
         CREATE MATERIALIZED VIEW mv AS SELECT id, a FROM t;
         CREATE VIEW v AS SELECT a FROM mv;
         UPDATE t SET a = 0 WHERE a IS NULL;
         ALTER TABLE t ALTER COLUMN a SET NOT NULL;",
    )
    .unwrap();
    assert_eq!(nullable(&db, "SELECT a FROM v"), [true]);
    db.apply_sql("REFRESH MATERIALIZED VIEW mv;").unwrap();
    assert_eq!(nullable(&db, "SELECT a FROM mv"), [false]);
    assert_eq!(nullable(&db, "SELECT a FROM v"), [false]);
    db.apply_sql("ALTER TABLE t ALTER COLUMN a DROP NOT NULL;")
        .unwrap();
    assert_eq!(nullable(&db, "SELECT a FROM mv"), [true]);
    assert_eq!(nullable(&db, "SELECT a FROM v"), [true]);
}
