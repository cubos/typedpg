//! Foreign tables and their wrappers' handlers. A wrapper without a
//! HANDLER can't scan or modify its tables: PREPARE succeeds, and every
//! execution fails (GetFdwRoutineByServerId: `foreign-data wrapper "w" has
//! no handler`) once the planner or the executor opens the routine. A
//! known handler lacking a callback (file_fdw's has no ExecForeignInsert,
//! ...) fails every execution that needs it.

use crate::common::*;

const NO_HANDLER: &str = "foreign-data wrapper \"w\" has no handler";

/// `ft` on a wrapper without a handler, a view over it, and an inheritance
/// tree and a partitioned table with a foreign member.
fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FOREIGN DATA WRAPPER w;
         CREATE SERVER s FOREIGN DATA WRAPPER w;
         CREATE FOREIGN TABLE ft (a int, b text) SERVER s;
         CREATE TABLE t (a int);
         CREATE VIEW v AS SELECT * FROM ft;
         CREATE TABLE ip (a int, b text);
         CREATE FOREIGN TABLE ic () INHERITS (ip) SERVER s;
         CREATE TABLE pt (a int, b text) PARTITION BY LIST (a);
         CREATE FOREIGN TABLE pt1 PARTITION OF pt FOR VALUES IN (1) SERVER s;
         CREATE TABLE pt2 PARTITION OF pt FOR VALUES IN (2);",
    )
    .unwrap();
    db
}

#[track_caller]
fn rejects(db: &PgCatalog, sql: &str, message: &str) {
    match db.analyze(sql) {
        Err(e) => assert!(
            e.to_string().starts_with(message),
            "{sql}\n  expected: {message}\n  got: {e}"
        ),
        Ok(_) => panic!("{sql}\n  expected: {message}\n  got: Ok"),
    }
}

#[track_caller]
fn accepts(db: &PgCatalog, sql: &str) {
    if let Err(e) = db.analyze(sql) {
        panic!("{sql}\n  expected: Ok\n  got: {e}");
    }
}

#[test]
fn a_wrapper_without_a_handler_fails_every_plan_reading_its_tables() {
    // get_relation_info (plancat.c) opens the routine of every foreign
    // table the planner plans — whatever the quals prove.
    let db = setup();
    for sql in [
        "SELECT * FROM ft",
        "SELECT * FROM ft WHERE false",
        "SELECT * FROM ft FOR UPDATE",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM ft)",
        "SELECT 1 FROM t WHERE a IN (SELECT a FROM ft)",
        "SELECT (SELECT a FROM ft LIMIT 1)",
        "SELECT a FROM t UNION ALL SELECT a FROM ft WHERE false",
        "SELECT * FROM t, LATERAL (SELECT * FROM ft WHERE ft.a = t.a) x",
        "INSERT INTO t SELECT a FROM ft WHERE false",
        "DELETE FROM t USING ft WHERE t.a = ft.a",
        // A view the rewriter expands, pulled up.
        "SELECT * FROM v",
        "SELECT 1 FROM v",
        // An inlined CTE, pulled up; a materialized one, read.
        "WITH c AS (SELECT * FROM ft) SELECT * FROM c",
        "WITH c AS (SELECT * FROM ft) SELECT 1 FROM c WHERE false",
        "WITH c AS MATERIALIZED (SELECT * FROM ft) SELECT 1 FROM c WHERE false",
        // An inheritance parent's children are all expanded.
        "SELECT * FROM ip",
        "SELECT * FROM ip WHERE false",
        "EXPLAIN SELECT * FROM ft",
    ] {
        rejects(&db, sql, NO_HANDLER);
    }
}

#[test]
fn a_foreign_table_the_planner_never_plans_needs_no_handler() {
    let db = setup();
    for sql in [
        // An unreferenced CTE is never planned (SS_process_ctes).
        "WITH c AS (SELECT * FROM ft) SELECT 1",
        "WITH c AS MATERIALIZED (SELECT * FROM ft) SELECT 1",
        // Constant folding drops the subquery before it is planned.
        "SELECT CASE WHEN false THEN (SELECT a FROM ft LIMIT 1) END",
        "SELECT 1 FROM t WHERE false AND (SELECT a FROM ft LIMIT 1) = 1",
        // A subquery planned on its own, proven empty first.
        "SELECT * FROM (SELECT a FROM ft LIMIT 1) s WHERE false",
        "SELECT * FROM (SELECT a FROM ft GROUP BY a) s WHERE false",
        // A removable join's nullable side (join_is_removable).
        "SELECT t.a FROM t LEFT JOIN (SELECT DISTINCT a FROM ft) x ON x.a = t.a",
        // ONLY reads no child; a partition may be pruned.
        "SELECT * FROM ONLY ip",
        "SELECT * FROM pt WHERE a = 2",
        "INSERT INTO ip VALUES (1)",
    ] {
        accepts(&db, sql);
    }
}

#[test]
fn a_wrapper_without_a_handler_fails_every_plan_writing_its_tables() {
    // make_modifytable opens a foreign result relation's routine, as does
    // the scan of an UPDATE / DELETE target — through a view too.
    let db = setup();
    for sql in [
        "INSERT INTO ft VALUES (1, 'x')",
        "INSERT INTO ft SELECT 1, 'x' WHERE false",
        "UPDATE ft SET a = 1 WHERE false",
        "DELETE FROM ft",
        "INSERT INTO v VALUES (1, 'x')",
        "UPDATE v SET a = 1 WHERE false",
        "DELETE FROM v",
        "WITH d AS (DELETE FROM ft RETURNING a) SELECT 1",
    ] {
        rejects(&db, sql, NO_HANDLER);
    }
}

#[test]
fn a_rule_action_on_a_foreign_table_is_planned_when_it_fires() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE r (a int);
         CREATE RULE rr AS ON INSERT TO r DO ALSO INSERT INTO ft VALUES (NEW.a, 'x');
         CREATE TABLE r2 (a int);
         CREATE RULE rr2 AS ON INSERT TO r2 WHERE NEW.a > 0
             DO ALSO DELETE FROM ft WHERE a = NEW.a;",
    )
    .unwrap();
    rejects(&db, "INSERT INTO r VALUES (1)", NO_HANDLER);
    rejects(&db, "INSERT INTO r2 VALUES (1)", NO_HANDLER);
    accepts(&db, "UPDATE r SET a = 1");
}

#[test]
fn truncate_needs_a_handler_supporting_it() {
    // truncate_check_rel: the named relation, then (without ONLY) each
    // inheritor.
    let db = setup();
    rejects(&db, "TRUNCATE ft", NO_HANDLER);
    rejects(&db, "TRUNCATE ip", NO_HANDLER);
    accepts(&db, "TRUNCATE ONLY ip");
    for sql in ["TRUNCATE ft;", "TRUNCATE ip;", "TRUNCATE pt;"] {
        let mut db = setup();
        let err = db.apply_sql(sql).unwrap_err();
        assert!(
            err.to_string().starts_with(NO_HANDLER),
            "{sql}\n  got: {err}"
        );
    }
    let mut db = setup();
    db.apply_sql("TRUNCATE ONLY ip;").unwrap();
}

#[test]
fn a_new_handler_or_none_changes_what_its_tables_can_do() {
    // ALTER FOREIGN DATA WRAPPER ... [NO] HANDLER.
    let mut db = setup();
    db.apply_sql(
        "CREATE EXTENSION file_fdw;
         ALTER FOREIGN DATA WRAPPER w HANDLER file_fdw_handler;
         ALTER FOREIGN TABLE ft OPTIONS (filename '/dev/null');",
    )
    .unwrap();
    accepts(&db, "SELECT * FROM ft");
    db.apply_sql("ALTER FOREIGN DATA WRAPPER w NO HANDLER;")
        .unwrap();
    rejects(&db, "SELECT * FROM ft", NO_HANDLER);
}

/// A file_fdw table, a view over it, and a table with a rule writing it.
fn file_fdw_setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE EXTENSION file_fdw;
         CREATE SERVER fs FOREIGN DATA WRAPPER file_fdw;
         CREATE FOREIGN TABLE f (a int, b text) SERVER fs OPTIONS (filename '/dev/null');
         CREATE VIEW fv AS SELECT * FROM f;
         CREATE TABLE r (a int);
         CREATE RULE rr AS ON INSERT TO r WHERE NEW.a > 0 DO ALSO INSERT INTO f VALUES (NEW.a, 'x');",
    )
    .unwrap();
    db
}

#[test]
fn file_fdw_reads_and_refuses_every_write() {
    // file_fdw_handler has no ExecForeignInsert / Update / Delete /
    // Truncate and no ImportForeignSchema: CheckValidResultRel,
    // truncate_check_rel and ImportForeignSchema refuse them whatever the
    // rows. Locking its rows copies them (ROW_MARK_COPY).
    let db = file_fdw_setup();
    for sql in [
        "SELECT * FROM f",
        "SELECT * FROM fv",
        "SELECT * FROM f FOR UPDATE",
    ] {
        accepts(&db, sql);
    }
    for (sql, message) in [
        (
            "INSERT INTO f VALUES (1, 'x')",
            "cannot insert into foreign table \"f\"",
        ),
        (
            "INSERT INTO f SELECT 1, 'x' WHERE false",
            "cannot insert into foreign table \"f\"",
        ),
        (
            "INSERT INTO fv VALUES (1, 'x')",
            "cannot insert into foreign table \"f\"",
        ),
        (
            "UPDATE f SET a = 1 WHERE false",
            "cannot update foreign table \"f\"",
        ),
        ("DELETE FROM fv", "cannot delete from foreign table \"f\""),
        (
            "INSERT INTO r VALUES (1)",
            "cannot insert into foreign table \"f\"",
        ),
        ("TRUNCATE f", "cannot truncate foreign table \"f\""),
    ] {
        rejects(&db, sql, message);
    }
}

#[test]
fn file_fdw_ddl_refuses_writes_and_import() {
    for (sql, message) in [
        ("TRUNCATE f;", "cannot truncate foreign table \"f\""),
        (
            "COPY f FROM STDIN;",
            "cannot insert into foreign table \"f\"",
        ),
        (
            "IMPORT FOREIGN SCHEMA x FROM SERVER fs INTO public;",
            "foreign-data wrapper \"file_fdw\" does not support IMPORT FOREIGN SCHEMA",
        ),
    ] {
        let mut db = file_fdw_setup();
        let err = db.apply_sql(sql).unwrap_err();
        assert!(
            err.to_string().starts_with(message),
            "{sql}\n  expected: {message}\n  got: {err}"
        );
    }
    let mut db = file_fdw_setup();
    db.apply_sql("ANALYZE f;").unwrap();
}

#[test]
fn ddl_reading_or_writing_a_table_of_a_wrapper_without_a_handler_fails() {
    for sql in [
        "ANALYZE ft;",
        "ANALYZE ip;",
        "ANALYZE ONLY ip;",
        "ANALYZE pt;",
        "COPY ft FROM STDIN;",
        "COPY (SELECT * FROM ft) TO STDOUT;",
        "CREATE MATERIALIZED VIEW mv AS SELECT * FROM ft;",
        "CREATE TABLE ct AS SELECT * FROM v;",
        "IMPORT FOREIGN SCHEMA x FROM SERVER s INTO public;",
    ] {
        let mut db = setup();
        let err = db.apply_sql(sql).unwrap_err();
        assert!(
            err.to_string().starts_with(NO_HANDLER),
            "{sql}\n  got: {err}"
        );
    }
    let mut db = setup();
    db.apply_sql(
        "CREATE MATERIALIZED VIEW mv AS SELECT * FROM ft WITH NO DATA;
         ANALYZE ONLY t;",
    )
    .unwrap();
    // VACUUM skips a foreign table (with a WARNING), its ANALYZE included.
    db.apply_sql("VACUUM ft;").unwrap();
}
