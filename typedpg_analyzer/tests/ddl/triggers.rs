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
             CREATE TRIGGER t6 AFTER UPDATE ON p FOR EACH STATEMENT WHEN (true) EXECUTE FUNCTION tf();",
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
