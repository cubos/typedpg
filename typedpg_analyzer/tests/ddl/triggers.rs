//! CREATE / DROP TRIGGER: the table and trigger function PG validates at
//! CREATE TRIGGER time, trigger names per table, and the trigger's
//! dependency on its function.

use crate::common::*;

#[test]
fn create_trigger_is_validated() {
    let setup = "CREATE TABLE t (a int);
                 CREATE VIEW v AS SELECT a FROM t;
                 CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
                 CREATE FUNCTION nf() RETURNS int LANGUAGE sql AS 'select 1';";
    for (stmt, msg) in [
        (
            "CREATE TRIGGER tr BEFORE INSERT ON nosuch FOR EACH ROW EXECUTE FUNCTION tf();",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION nosuch();",
            "function nosuch() does not exist",
        ),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION nf();",
            "function nf must return type trigger",
        ),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();
             CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();",
            "trigger \"tr\" for relation \"t\" already exists",
        ),
        (
            "CREATE TRIGGER tv BEFORE INSERT ON v FOR EACH ROW EXECUTE FUNCTION tf();",
            "\"v\" is a view",
        ),
        (
            "CREATE TRIGGER ti INSTEAD OF INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();",
            "\"t\" is a table",
        ),
        (
            "DROP TRIGGER nosuch ON t;",
            "trigger \"nosuch\" for table \"t\" does not exist",
        ),
        (
            "DROP TRIGGER tr ON nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();
             DROP FUNCTION tf();",
            "cannot drop function tf() because other objects depend on it",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();
             CREATE OR REPLACE TRIGGER tr AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();
             CREATE TRIGGER tvi INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION tf();
             CREATE TRIGGER tvs AFTER INSERT ON v FOR EACH STATEMENT EXECUTE FUNCTION tf();
             DROP TRIGGER IF EXISTS nosuch ON t;
             DROP TRIGGER tr ON t;
             DROP FUNCTION tf() CASCADE;",
        ),
    ]);
}

#[test]
fn alter_trigger_rename_is_tracked() {
    let setup = "CREATE TABLE t (a int);
                 CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
                 CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();
                 CREATE TRIGGER tr2 BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();";
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TRIGGER tr ON t RENAME TO x; DROP TRIGGER x ON t;",
        ),
    ]);
    for (stmt, msg) in [
        (
            "ALTER TRIGGER tr ON t RENAME TO tr2;",
            "trigger \"tr2\" for relation \"t\" already exists",
        ),
        (
            "ALTER TRIGGER nosuch ON t RENAME TO z;",
            "trigger \"nosuch\" for table \"t\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
}

#[test]
fn trigger_when_conditions_reference_only_the_available_row_values() {
    // CreateTriggerFiringOn (PG 18): the WHEN condition is a boolean over
    // OLD / NEW, restricted by the trigger's level, events and timing.
    let setup = "CREATE TABLE g (a int, b int GENERATED ALWAYS AS (a) VIRTUAL,
                   c int GENERATED ALWAYS AS (a) STORED);
                 CREATE TABLE p (a int, b int);
                 CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';";
    let generated = "BEFORE trigger's WHEN condition cannot reference NEW generated columns";
    for (stmt, msg) in [
        (
            "BEFORE INSERT ON g FOR EACH ROW WHEN (new.b > 0)",
            generated,
        ),
        (
            "BEFORE INSERT ON g FOR EACH ROW WHEN (new.c > 0)",
            generated,
        ),
        (
            "BEFORE INSERT ON g FOR EACH ROW WHEN (new.* IS NOT NULL)",
            generated,
        ),
        (
            "BEFORE INSERT ON g FOR EACH ROW WHEN (new IS NOT NULL)",
            generated,
        ),
        (
            "BEFORE INSERT ON p FOR EACH ROW WHEN (old.a > 0)",
            "INSERT trigger's WHEN condition cannot reference OLD values",
        ),
        (
            "BEFORE DELETE ON p FOR EACH ROW WHEN (new.a > 0)",
            "DELETE trigger's WHEN condition cannot reference NEW values",
        ),
        (
            "BEFORE INSERT ON p FOR EACH STATEMENT WHEN (new.a > 0)",
            "statement trigger's WHEN condition cannot reference column values",
        ),
        (
            "BEFORE INSERT ON p FOR EACH ROW WHEN (new.xmin::text = '1')",
            "BEFORE trigger's WHEN condition cannot reference NEW system columns",
        ),
        // A field selected from the row is that column (transformIndirection).
        (
            "BEFORE UPDATE ON p FOR EACH ROW WHEN ((new).ctid IS NULL)",
            "BEFORE trigger's WHEN condition cannot reference NEW system columns",
        ),
        (
            "BEFORE INSERT ON g FOR EACH ROW WHEN ((new).c > 0)",
            generated,
        ),
        (
            "BEFORE INSERT ON p FOR EACH ROW WHEN (a > 0)",
            "column reference \"a\" is ambiguous",
        ),
        (
            "BEFORE INSERT ON p FOR EACH ROW WHEN (new.a)",
            "argument of WHEN must be type boolean, not type integer",
        ),
        (
            "BEFORE INSERT ON p FOR EACH ROW WHEN ((SELECT true))",
            "cannot use subquery in trigger WHEN condition",
        ),
        (
            "BEFORE INSERT ON p FOR EACH ROW WHEN (count(*) > 0)",
            "aggregate functions are not allowed in trigger WHEN conditions",
        ),
        (
            "BEFORE INSERT ON p FOR EACH ROW WHEN (generate_series(1, 2) > 0)",
            "set-returning functions are not allowed in trigger WHEN conditions",
        ),
    ] {
        let stmt = format!("CREATE TRIGGER tr {stmt} EXECUTE FUNCTION tf();");
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", &stmt)]).expect_err(&stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TRIGGER t1 AFTER INSERT ON g FOR EACH ROW WHEN (new.b > 0) EXECUTE FUNCTION tf();
             CREATE TRIGGER t2 BEFORE UPDATE ON g FOR EACH ROW WHEN (old.b > 0) EXECUTE FUNCTION tf();
             CREATE TRIGGER t3 AFTER INSERT ON p FOR EACH ROW WHEN (new.xmin::text = '1') EXECUTE FUNCTION tf();
             CREATE TRIGGER t4 BEFORE UPDATE ON p FOR EACH ROW WHEN (old.a IS DISTINCT FROM new.a) EXECUTE FUNCTION tf();
             CREATE TRIGGER t5 BEFORE INSERT ON p FOR EACH ROW WHEN (new IS NOT NULL) EXECUTE FUNCTION tf();
             CREATE TRIGGER t6 AFTER UPDATE ON p FOR EACH STATEMENT WHEN (true) EXECUTE FUNCTION tf();
             CREATE TRIGGER t7 BEFORE UPDATE ON p FOR EACH ROW WHEN ((old).ctid IS NULL) EXECUTE FUNCTION tf();
             CREATE TRIGGER t8 BEFORE INSERT ON g FOR EACH ROW WHEN ((new).a > 0) EXECUTE FUNCTION tf();
             CREATE TRIGGER t9 AFTER UPDATE ON p FOR EACH ROW WHEN ((new).ctid IS NULL) EXECUTE FUNCTION tf();",
        ),
    ]);
}

#[test]
fn trigger_firing_transition_tables_and_columns_are_validated() {
    // PG 18 CreateTriggerFiringOn: the trigger type, the REFERENCING
    // clause, the UPDATE OF columns, constraint triggers.
    let setup = "CREATE TABLE t (a int);
                 CREATE TABLE c (a int) INHERITS (t);
                 CREATE VIEW v AS SELECT a FROM t;
                 CREATE TABLE p (a int) PARTITION BY LIST (a);
                 CREATE TABLE p1 PARTITION OF p FOR VALUES IN (1);
                 CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
                 CREATE CONSTRAINT TRIGGER ct AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();";
    for (stmt, msg) in [
        (
            "CREATE TRIGGER tr BEFORE TRUNCATE ON t FOR EACH ROW EXECUTE FUNCTION tf();",
            "TRUNCATE FOR EACH ROW triggers are not supported",
        ),
        (
            "CREATE TRIGGER tr INSTEAD OF UPDATE OF a ON v FOR EACH ROW EXECUTE FUNCTION tf();",
            "INSTEAD OF triggers cannot have column lists",
        ),
        (
            "CREATE TRIGGER tr INSTEAD OF UPDATE ON v FOR EACH STATEMENT EXECUTE FUNCTION tf();",
            "INSTEAD OF triggers must be FOR EACH ROW",
        ),
        (
            "CREATE TRIGGER tr INSTEAD OF UPDATE ON v FOR EACH ROW WHEN (true) EXECUTE FUNCTION tf();",
            "INSTEAD OF triggers cannot have WHEN conditions",
        ),
        (
            "CREATE TRIGGER tr AFTER TRUNCATE ON v EXECUTE FUNCTION tf();",
            "\"v\" is a view",
        ),
        (
            "CREATE TRIGGER tr AFTER UPDATE OF a, a ON t FOR EACH ROW EXECUTE FUNCTION tf();",
            "column \"a\" specified more than once",
        ),
        (
            "CREATE TRIGGER tr AFTER UPDATE OF nope ON t FOR EACH ROW EXECUTE FUNCTION tf();",
            "column \"nope\" of relation \"t\" does not exist",
        ),
        (
            "CREATE TRIGGER tr AFTER INSERT ON t REFERENCING OLD TABLE o
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "OLD TABLE can only be specified for a DELETE or UPDATE trigger",
        ),
        (
            "CREATE TRIGGER tr AFTER DELETE ON t REFERENCING NEW TABLE n
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "NEW TABLE can only be specified for an INSERT or UPDATE trigger",
        ),
        (
            "CREATE TRIGGER tr BEFORE DELETE ON t REFERENCING OLD TABLE o
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "transition table name can only be specified for an AFTER trigger",
        ),
        (
            "CREATE TRIGGER tr AFTER UPDATE OF a ON t REFERENCING OLD TABLE o
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "transition tables cannot be specified for triggers with column lists",
        ),
        (
            "CREATE TRIGGER tr AFTER UPDATE OR DELETE ON t REFERENCING OLD TABLE o
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "transition tables cannot be specified for triggers with more than one event",
        ),
        (
            "CREATE TRIGGER tr AFTER UPDATE ON t REFERENCING OLD TABLE o NEW TABLE o
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "OLD TABLE name and NEW TABLE name cannot be the same",
        ),
        (
            "CREATE TRIGGER tr AFTER UPDATE ON p REFERENCING OLD TABLE o
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "\"p\" is a partitioned table",
        ),
        (
            "CREATE TRIGGER tr AFTER UPDATE ON p1 REFERENCING OLD TABLE o
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "ROW triggers with transition tables are not supported on partitions",
        ),
        (
            "CREATE TRIGGER tr AFTER UPDATE ON c REFERENCING OLD TABLE o
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "ROW triggers with transition tables are not supported on inheritance children",
        ),
        (
            "CREATE TRIGGER tr AFTER INSERT ON v REFERENCING NEW TABLE n
             FOR EACH STATEMENT EXECUTE FUNCTION tf();",
            "\"v\" is a view",
        ),
        (
            "CREATE OR REPLACE TRIGGER ct AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();",
            "trigger \"ct\" for relation \"t\" is a constraint trigger",
        ),
        (
            "CREATE CONSTRAINT TRIGGER ct2 AFTER INSERT ON t FROM nosuch
             FOR EACH ROW EXECUTE FUNCTION tf();",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE TRIGGER tr AFTER INSERT ON pg_catalog.pg_class EXECUTE FUNCTION tf();",
            "permission denied: \"pg_class\" is a system catalog",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TRIGGER t1 AFTER UPDATE OF a ON t FOR EACH ROW EXECUTE FUNCTION tf();
             CREATE TRIGGER t2 AFTER INSERT ON t REFERENCING NEW TABLE n
                 FOR EACH ROW EXECUTE FUNCTION tf();
             CREATE TRIGGER t3 AFTER UPDATE ON t REFERENCING OLD TABLE o NEW TABLE n
                 FOR EACH STATEMENT EXECUTE FUNCTION tf();
             CREATE TRIGGER t4 AFTER UPDATE ON p REFERENCING OLD TABLE o
                 FOR EACH STATEMENT EXECUTE FUNCTION tf();
             CREATE TRIGGER t5 AFTER TRUNCATE ON t EXECUTE FUNCTION tf();
             CREATE CONSTRAINT TRIGGER ct2 AFTER INSERT ON t FROM p
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION tf();
             BEGIN; SET CONSTRAINTS ct2 DEFERRED; COMMIT;",
        ),
    ]);
}

#[test]
fn row_triggers_of_a_partitioned_table_are_cloned_to_its_partitions() {
    // PG 18 CreateTriggerFiringOn (recursing to the partitions),
    // CloneRowTriggersToPartition, DropClonedTriggersFromPartition,
    // renametrig and the clones' dependency on their parent's trigger.
    let setup = "CREATE TABLE t (a int) PARTITION BY LIST (a);
                 CREATE TABLE t1 PARTITION OF t FOR VALUES IN (1);
                 CREATE TABLE t3 PARTITION OF t FOR VALUES IN (3) PARTITION BY LIST (a);
                 CREATE TABLE t31 PARTITION OF t3 FOR VALUES IN (3);
                 CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
                 CREATE TRIGGER tr AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();
                 CREATE TRIGGER st AFTER INSERT ON t FOR EACH STATEMENT EXECUTE FUNCTION tf();
                 CREATE TABLE t2 (a int);
                 CREATE TRIGGER tr AFTER INSERT ON t2 FOR EACH ROW EXECUTE FUNCTION tf();";
    for (stmt, msg) in [
        (
            "CREATE TRIGGER tr AFTER INSERT ON t1 FOR EACH ROW EXECUTE FUNCTION tf();",
            "trigger \"tr\" for relation \"t1\" already exists",
        ),
        (
            "CREATE OR REPLACE TRIGGER tr AFTER INSERT ON t1 FOR EACH ROW EXECUTE FUNCTION tf();",
            "trigger \"tr\" for relation \"t1\" is an internal or a child trigger",
        ),
        (
            "DROP TRIGGER tr ON t1;",
            "cannot drop trigger tr on table t1 because trigger tr on table t requires it",
        ),
        (
            "DROP TRIGGER tr ON t31;",
            "cannot drop trigger tr on table t31 because trigger tr on table t3 requires it",
        ),
        (
            "ALTER TRIGGER tr ON t1 RENAME TO tr2;",
            "cannot rename trigger \"tr\" on table \"t1\"",
        ),
        (
            "ALTER TABLE t ATTACH PARTITION t2 FOR VALUES IN (2);",
            "trigger \"tr\" for relation \"t2\" already exists",
        ),
        (
            "CREATE TABLE t4 PARTITION OF t FOR VALUES IN (4); DROP TRIGGER tr ON t4;",
            "cannot drop trigger tr on table t4 because trigger tr on table t requires it",
        ),
        (
            "ALTER TRIGGER tr ON t RENAME TO tr2; DROP TRIGGER tr2 ON t31;",
            "cannot drop trigger tr2 on table t31 because trigger tr2 on table t3 requires it",
        ),
        (
            "CREATE TRIGGER tr1 AFTER INSERT ON t1 FOR EACH ROW EXECUTE FUNCTION tf();
             CREATE TRIGGER tr1 AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();",
            "trigger \"tr1\" for relation \"t1\" already exists",
        ),
        (
            "ALTER TABLE t DETACH PARTITION t1; DROP TRIGGER tr ON t1;",
            "trigger \"tr\" for table \"t1\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TRIGGER st AFTER INSERT ON t1 FOR EACH STATEMENT EXECUTE FUNCTION tf();
             CREATE OR REPLACE TRIGGER tr AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION tf();
             ALTER TRIGGER tr ON t RENAME TO tr2;
             DROP TRIGGER tr2 ON t;
             CREATE TRIGGER tr2 AFTER INSERT ON t31 FOR EACH ROW EXECUTE FUNCTION tf();
             DROP TRIGGER tr ON t2;
             ALTER TABLE t ATTACH PARTITION t2 FOR VALUES IN (2);",
        ),
    ]);
}

#[test]
fn drop_trigger_if_exists_skips_a_missing_table() {
    // PG 18 get_object_address_relobject: with IF EXISTS, a missing
    // relation or schema is skipped with a notice.
    build_db(&[(
        "0001.sql",
        "DROP TRIGGER IF EXISTS tr ON nosuch;
         DROP TRIGGER IF EXISTS tr ON nosch.nosuch;
         DROP POLICY IF EXISTS p ON nosuch;",
    )]);
}

#[test]
fn alter_trigger_depends_on_extension_looks_the_trigger_up() {
    // ExecAlterObjectDependsStmt: get_object_address_rv opens the table
    // (table_openrv) and finds the trigger, then the extension is looked
    // up. The same for a routine, an index and a materialized view.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE u (x int);
         CREATE TABLE t (a int);
         CREATE INDEX t_a ON t (a);
         CREATE TYPE ct AS (a int);
         CREATE VIEW v AS SELECT 1 AS x;
         CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a;
         CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TRIGGER tg BEFORE INSERT ON u FOR EACH ROW EXECUTE FUNCTION trf();",
    )]);
    db.apply_sql(
        "ALTER TRIGGER tg ON u DEPENDS ON EXTENSION plpgsql;
         ALTER TRIGGER tg ON u DEPENDS ON EXTENSION plpgsql;
         ALTER TRIGGER tg ON u NO DEPENDS ON EXTENSION plpgsql;
         ALTER TRIGGER tg ON u NO DEPENDS ON EXTENSION plpgsql;
         ALTER FUNCTION trf() DEPENDS ON EXTENSION plpgsql;
         ALTER ROUTINE trf() NO DEPENDS ON EXTENSION plpgsql;
         ALTER INDEX t_a DEPENDS ON EXTENSION plpgsql;
         ALTER MATERIALIZED VIEW mv DEPENDS ON EXTENSION plpgsql;",
    )
    .unwrap();
    for (sql, message) in [
        (
            "ALTER TRIGGER nosuch ON u DEPENDS ON EXTENSION plpgsql;",
            "trigger \"nosuch\" for table \"u\" does not exist",
        ),
        (
            "ALTER TRIGGER nosuch ON u NO DEPENDS ON EXTENSION plpgsql;",
            "trigger \"nosuch\" for table \"u\" does not exist",
        ),
        (
            "ALTER TRIGGER nosuch ON u DEPENDS ON EXTENSION nosuch;",
            "trigger \"nosuch\" for table \"u\" does not exist",
        ),
        (
            "ALTER TRIGGER tg ON u DEPENDS ON EXTENSION nosuch;",
            "extension \"nosuch\" does not exist",
        ),
        (
            "ALTER TRIGGER tg ON u NO DEPENDS ON EXTENSION nosuch;",
            "extension \"nosuch\" does not exist",
        ),
        (
            "ALTER TRIGGER tg ON nosuch DEPENDS ON EXTENSION nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER TRIGGER tg ON nosch.u DEPENDS ON EXTENSION plpgsql;",
            "schema \"nosch\" does not exist",
        ),
        (
            "ALTER TRIGGER tg ON t_a DEPENDS ON EXTENSION plpgsql;",
            "cannot open relation \"t_a\" (This operation is not supported for indexes.)",
        ),
        (
            "ALTER TRIGGER tg ON ct DEPENDS ON EXTENSION plpgsql;",
            "cannot open relation \"ct\"",
        ),
        (
            "ALTER TRIGGER tg ON v DEPENDS ON EXTENSION plpgsql;",
            "trigger \"tg\" for table \"v\" does not exist",
        ),
        (
            "ALTER TRIGGER tg ON pg_class DEPENDS ON EXTENSION plpgsql;",
            "trigger \"tg\" for table \"pg_class\" does not exist",
        ),
        (
            "ALTER FUNCTION nosuch() DEPENDS ON EXTENSION nosuch;",
            "function nosuch() does not exist",
        ),
        (
            "ALTER FUNCTION trf() DEPENDS ON EXTENSION nosuch;",
            "extension \"nosuch\" does not exist",
        ),
        (
            "ALTER PROCEDURE trf() DEPENDS ON EXTENSION plpgsql;",
            "trf() is not a procedure",
        ),
        (
            "ALTER INDEX nosuch DEPENDS ON EXTENSION nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER INDEX u DEPENDS ON EXTENSION plpgsql;",
            "\"u\" is not an index",
        ),
        (
            "ALTER INDEX t_a DEPENDS ON EXTENSION nosuch;",
            "extension \"nosuch\" does not exist",
        ),
        (
            "ALTER MATERIALIZED VIEW t_a DEPENDS ON EXTENSION plpgsql;",
            "\"t_a\" is not a materialized view",
        ),
    ] {
        let err = db.apply_sql(sql).expect_err(sql);
        assert!(
            err.to_string().starts_with(message),
            "{sql}\n  expected: {message}\n       got: {err}"
        );
    }
}

#[test]
fn drop_extension_takes_along_what_depends_on_it_automatically() {
    // DEPENDENCY_AUTO_EXTENSION: DROP EXTENSION drops the object without
    // CASCADE; what depends on the object normally still needs it, and a
    // partition's trigger clone goes only with its parent's. (CREATE
    // EXTENSION leaves the pg_sanity mirror out, so the expectations here
    // were checked by hand against PostgreSQL 18.6.)
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION hstore;
         CREATE TABLE u (x int NOT NULL);
         CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql
             AS 'BEGIN NEW.x := 1; RETURN NEW; END';
         CREATE TRIGGER tg BEFORE INSERT ON u FOR EACH ROW EXECUTE FUNCTION trf();
         CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a;
         CREATE VIEW vv AS SELECT * FROM mv;
         CREATE TABLE p (a int) PARTITION BY RANGE (a);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (1) TO (2);
         CREATE TRIGGER ptg BEFORE INSERT ON p FOR EACH ROW EXECUTE FUNCTION trf();
         CREATE INDEX ui ON u (x);
         ALTER TABLE u ADD CONSTRAINT upk PRIMARY KEY (x);",
    )]);
    let blocked = "cannot drop extension hstore because other objects depend on it";
    let expect = |db: &mut PgCatalog, sql: &str, message: &str| {
        let err = db.apply_sql(sql).expect_err(sql);
        assert!(
            err.to_string().starts_with(message),
            "{sql}\n  expected: {message}\n       got: {err}"
        );
    };
    db.apply_sql("ALTER MATERIALIZED VIEW mv DEPENDS ON EXTENSION hstore;")
        .unwrap();
    expect(&mut db, "DROP EXTENSION hstore;", blocked);
    db.apply_sql(
        "DROP VIEW vv;
         ALTER FUNCTION trf() DEPENDS ON EXTENSION hstore;",
    )
    .unwrap();
    expect(&mut db, "DROP EXTENSION hstore;", blocked);
    db.apply_sql(
        "ALTER TRIGGER tg ON u DEPENDS ON EXTENSION hstore;
         ALTER FUNCTION trf() NO DEPENDS ON EXTENSION hstore;
         ALTER TRIGGER ptg ON p1 DEPENDS ON EXTENSION hstore;",
    )
    .unwrap();
    expect(
        &mut db,
        "DROP EXTENSION hstore;",
        "cannot drop trigger ptg on table p1 because trigger ptg on table p requires it",
    );
    db.apply_sql(
        "ALTER TRIGGER ptg ON p1 NO DEPENDS ON EXTENSION hstore;
         ALTER INDEX upk DEPENDS ON EXTENSION hstore;",
    )
    .unwrap();
    expect(
        &mut db,
        "DROP EXTENSION hstore;",
        "cannot drop extension hstore because other objects depend on it (constraint upk on \
         table u depends on extension hstore)",
    );
    db.apply_sql(
        "ALTER INDEX upk NO DEPENDS ON EXTENSION hstore;
         ALTER INDEX ui DEPENDS ON EXTENSION hstore;
         ALTER TRIGGER ptg ON p DEPENDS ON EXTENSION hstore;
         DROP EXTENSION hstore;
         CREATE TRIGGER tg BEFORE INSERT ON u FOR EACH ROW EXECUTE FUNCTION trf();
         CREATE TRIGGER ptg BEFORE INSERT ON p FOR EACH ROW EXECUTE FUNCTION trf();
         CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a;
         CREATE INDEX ui ON u (x);",
    )
    .unwrap();
    // CASCADE takes along what depends on them.
    db.apply_sql(
        "CREATE EXTENSION hstore;
         CREATE FUNCTION f2() RETURNS int LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f2() DEPENDS ON EXTENSION hstore;
         CREATE VIEW v2 AS SELECT f2();",
    )
    .unwrap();
    expect(&mut db, "DROP EXTENSION hstore;", blocked);
    db.apply_sql("DROP EXTENSION hstore CASCADE; CREATE VIEW v2 AS SELECT 1;")
        .unwrap();
}

#[test]
fn drop_extension_and_the_marks_of_rebuilt_and_partition_objects() {
    // A partition's index attached to its parent's goes only with it, like
    // a trigger clone: marking it alone blocks DROP EXTENSION, CASCADE or
    // not. A clone may be marked without its parent. An index ALTER COLUMN
    // TYPE rebuilds is a new object, without the old one's mark; REINDEX
    // keeps it. (CREATE EXTENSION leaves the pg_sanity mirror out, so the
    // expectations here were checked by hand against PostgreSQL 18.6.)
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION hstore;
         CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TABLE q (a int NOT NULL, b int) PARTITION BY RANGE (a);
         CREATE TABLE q1 PARTITION OF q FOR VALUES FROM (1) TO (2);
         CREATE INDEX pj ON q (a);
         CREATE TRIGGER qtg BEFORE INSERT ON q FOR EACH ROW EXECUTE FUNCTION trf();
         CREATE TABLE t (a int, b int);
         CREATE INDEX i ON t (a);
         CREATE INDEX k ON t (b) WHERE a > 0;
         CREATE INDEX j ON t (b);",
    )]);
    let expect = |db: &mut PgCatalog, sql: &str, message: &str| {
        let err = db.apply_sql(sql).expect_err(sql);
        assert!(
            err.to_string().starts_with(message),
            "{sql}\n  expected: {message}\n       got: {err}"
        );
    };
    let clone_blocked = "cannot drop index q1_a_idx because index pj requires it";
    db.apply_sql("ALTER INDEX q1_a_idx DEPENDS ON EXTENSION hstore;")
        .unwrap();
    expect(&mut db, "DROP EXTENSION hstore;", clone_blocked);
    expect(&mut db, "DROP EXTENSION hstore CASCADE;", clone_blocked);
    db.apply_sql(
        "ALTER INDEX q1_a_idx NO DEPENDS ON EXTENSION hstore;
         ALTER TABLE q ADD PRIMARY KEY (a);
         ALTER INDEX q1_pkey DEPENDS ON EXTENSION hstore;",
    )
    .unwrap();
    expect(
        &mut db,
        "DROP EXTENSION hstore;",
        "cannot drop index q1_pkey because index q_pkey requires it",
    );
    // An index ATTACH PARTITION attaches is the parent's too.
    db.apply_sql(
        "ALTER INDEX q1_pkey NO DEPENDS ON EXTENSION hstore;
         CREATE TABLE r (a int NOT NULL, b int);
         CREATE INDEX ri ON r (a);
         ALTER INDEX ri DEPENDS ON EXTENSION hstore;
         ALTER TABLE q ATTACH PARTITION r FOR VALUES FROM (5) TO (6);",
    )
    .unwrap();
    expect(
        &mut db,
        "DROP EXTENSION hstore;",
        "cannot drop index ri because index pj requires it",
    );
    db.apply_sql(
        "ALTER INDEX ri NO DEPENDS ON EXTENSION hstore;
         ALTER TRIGGER qtg ON q1 DEPENDS ON EXTENSION hstore;",
    )
    .unwrap();
    expect(
        &mut db,
        "DROP EXTENSION hstore;",
        "cannot drop trigger qtg on table q1 because trigger qtg on table q requires it",
    );
    // With the parents marked too, everything goes.
    db.apply_sql(
        "ALTER INDEX q1_a_idx DEPENDS ON EXTENSION hstore;
         ALTER INDEX pj DEPENDS ON EXTENSION hstore;
         ALTER TRIGGER qtg ON q DEPENDS ON EXTENSION hstore;
         ALTER INDEX i DEPENDS ON EXTENSION hstore;
         ALTER INDEX k DEPENDS ON EXTENSION hstore;
         ALTER INDEX j DEPENDS ON EXTENSION hstore;
         ALTER TABLE t ALTER COLUMN a TYPE bigint;
         REINDEX INDEX j;
         DROP EXTENSION hstore;
         ALTER INDEX i RENAME TO i2;
         ALTER INDEX k RENAME TO k2;
         CREATE INDEX pj ON q (a);
         CREATE TRIGGER qtg BEFORE INSERT ON q FOR EACH ROW EXECUTE FUNCTION trf();",
    )
    .unwrap();
    expect(
        &mut db,
        "ALTER INDEX j RENAME TO j2;",
        "relation \"j\" does not exist",
    );
}

#[test]
fn drop_extension_takes_a_marked_partitioned_index_with_its_partitions_indexes() {
    // A partition's index is part of its parent's (DEPENDENCY_PARTITION_PRI):
    // with only the parent marked DEPENDS ON EXTENSION, DROP EXTENSION drops
    // the clones too. (Checked by hand against PostgreSQL 18.6, CREATE
    // EXTENSION leaving the pg_sanity mirror out.)
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION hstore;
         CREATE TABLE p (a int, b int) PARTITION BY RANGE (a);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (1) TO (2) PARTITION BY RANGE (a);
         CREATE TABLE p11 PARTITION OF p1 FOR VALUES FROM (1) TO (2);
         CREATE INDEX pi ON p (b);
         ALTER INDEX pi DEPENDS ON EXTENSION hstore;
         DROP EXTENSION hstore;",
    )]);
    for index in ["pi", "p1_b_idx", "p11_b_idx"] {
        let sql = format!("ALTER INDEX {index} RENAME TO x;");
        let err = db.apply_sql(&sql).expect_err(&sql);
        assert!(
            err.to_string()
                .starts_with(&format!("relation \"{index}\" does not exist")),
            "{sql}\n  got: {err}"
        );
    }
    db.apply_sql("CREATE INDEX pi ON p (b);").unwrap();
}
