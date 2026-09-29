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
