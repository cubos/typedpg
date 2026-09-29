//! ALTER TABLE: ADD/DROP/SET/ALTER column, RENAME COLUMN, DROP CONSTRAINT,
//! ADD PRIMARY KEY, ALTER COLUMN TYPE, SET DEFAULT / DROP DEFAULT.

use crate::common::*;

// ── ADD / DROP / SET / ALTER COLUMN ─────────────────────────────────────────

#[test]
fn alter_table_add_column() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "ALTER TABLE t ADD COLUMN name TEXT;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    assert_eq!(attrs.len(), 2);
    assert_eq!(attrs[1].attname, "name");
    assert!(!attrs[1].attnotnull);
}

#[test]
fn alter_table_drop_column() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT, age INT);",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN name;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    assert_eq!(attrs.len(), 2);
    assert_eq!(attrs[0].attname, "id");
    assert_eq!(attrs[1].attname, "age");
}

#[test]
fn alter_table_set_not_null() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT, name TEXT);"),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN name SET NOT NULL;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let name_col = attrs.iter().find(|c| c.attname == "name").unwrap();
    assert!(name_col.attnotnull);
}

#[test]
fn alter_table_drop_not_null() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN name DROP NOT NULL;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let name_col = attrs.iter().find(|c| c.attname == "name").unwrap();
    assert!(!name_col.attnotnull);
}

#[test]
fn alter_table_set_default() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, status TEXT NOT NULL);",
        ),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN status SET DEFAULT 'active';",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let status_col = attrs.iter().find(|c| c.attname == "status").unwrap();
    assert!(status_col.atthasdef);
}

#[test]
fn alter_table_alter_column_type() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL, amount INT);"),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN amount TYPE BIGINT;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let amount_col = attrs.iter().find(|c| c.attname == "amount").unwrap();
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(amount_col.atttypid, int8_oid);
}

// ── ALTER COLUMN TYPE with dependent views ─────────────────────────────────
//
// PG (SQLSTATE 0A000) blocks `ALTER COLUMN TYPE` on any column referenced by a
// view, even when the change would be binary-coercible. We mirror that — the
// only safe migration is DROP VIEW → ALTER → CREATE VIEW.

#[test]
fn alter_column_type_with_view_fails_even_when_binary_coercible() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE DOMAIN user_id AS INT;
             CREATE TABLE t (id user_id NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN id TYPE INT;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cannot alter type of a column used by a view or rule: column t.id is referenced by view(s) public.v (hint: drop the view(s) first, alter the column, then recreate)",
    );
}

#[test]
fn alter_column_type_with_view_fails_when_not_binary_coercible() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, amount INT);
             CREATE VIEW v AS SELECT amount FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN amount TYPE BIGINT;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cannot alter type of a column used by a view or rule: column t.amount is referenced by view(s) public.v (hint: drop the view(s) first, alter the column, then recreate)",
    );
}

// ── ADD COLUMN and IF NOT EXISTS ──────────────────────────────────────────

#[test]
fn alter_table_add_column_duplicate_errors() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        ("0002.sql", "ALTER TABLE t ADD COLUMN name TEXT;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "column \"name\" of relation \"t\" already exists"
    );
}

#[test]
fn alter_table_add_column_if_not_exists_on_existing_is_noop() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        (
            "0002.sql",
            "ALTER TABLE t ADD COLUMN IF NOT EXISTS name TEXT;",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(
        snap.attributes_of(table.oid).len(),
        2,
        "should still have 2 columns"
    );
}

// ── ADD CONSTRAINT PRIMARY KEY ────────────────────────────────────────────

#[test]
fn alter_table_add_primary_key_sets_not_null() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT, name TEXT);"),
        (
            "0002.sql",
            "ALTER TABLE t ADD CONSTRAINT t_pkey PRIMARY KEY (id);",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = attrs.iter().find(|c| c.attname == "id").unwrap();
    assert!(
        id_col.attnotnull,
        "PRIMARY KEY constraint must make the column NOT NULL"
    );
}

// ── DROP CONSTRAINT ───────────────────────────────────────────────────────

#[test]
fn alter_table_drop_constraint_if_exists_is_noop() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL UNIQUE);",
        ),
        (
            "0002.sql",
            "ALTER TABLE t DROP CONSTRAINT IF EXISTS t_name_key;",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(snap.attributes_of(table.oid).len(), 2);
}

// ── Multiple ALTER commands in one statement ──────────────────────────────

#[test]
fn alter_table_multiple_commands_in_one_statement() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT, age INT);",
        ),
        (
            "0002.sql",
            "ALTER TABLE t
                ALTER COLUMN name SET NOT NULL,
                ALTER COLUMN age SET DEFAULT 0;",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let name = attrs.iter().find(|c| c.attname == "name").unwrap();
    let age = attrs.iter().find(|c| c.attname == "age").unwrap();
    assert!(name.attnotnull);
    assert!(age.atthasdef);
}

// ── Errors on nonexistent column ──────────────────────────────────────────

#[test]
fn alter_column_set_not_null_on_nonexistent_column_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN nonexistent SET NOT NULL;",
        ),
    ]);

    assert_ddl_err!(
        result,
        DdlError::Parse(_),
        "column \"nonexistent\" of relation \"t\" does not exist"
    );
}

#[test]
fn alter_column_set_default_on_nonexistent_column_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN ghost SET DEFAULT 42;",
        ),
    ]);

    assert_ddl_err!(
        result,
        DdlError::Parse(_),
        "column \"ghost\" of relation \"t\" does not exist"
    );
}

#[test]
fn alter_column_type_on_nonexistent_column_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN ghost TYPE BIGINT;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::Parse(_),
        "column \"ghost\" of relation \"t\" does not exist"
    );
}

// ── ALTER COLUMN TYPE validation (ATPrepAlterColumnType / ATExecAlterColumnType)

#[test]
fn alter_column_type_requires_an_assignment_cast_or_using() {
    for (sql, msg) in [
        (
            "CREATE TABLE t (a text); ALTER TABLE t ALTER COLUMN a TYPE int;",
            "column \"a\" cannot be cast automatically to type integer",
        ),
        (
            "CREATE TABLE t4 (x text DEFAULT 'a'); ALTER TABLE t4 ALTER x TYPE int USING length(x);",
            "default for column \"x\" cannot be cast automatically to type integer",
        ),
        (
            "CREATE TABLE t5 (x int DEFAULT 1); ALTER TABLE t5 ALTER x TYPE bool USING x > 0;",
            "default for column \"x\" cannot be cast automatically to type boolean",
        ),
        (
            "CREATE TABLE t7 (x serial); ALTER TABLE t7 ALTER x TYPE bool USING x > 0;",
            "default for column \"x\" cannot be cast automatically to type boolean",
        ),
        (
            "CREATE TABLE t6 (x int); ALTER TABLE t6 ALTER x TYPE date USING 'abc';",
            "invalid input syntax for type date: \"abc\"",
        ),
        (
            "CREATE TABLE t6 (x int); ALTER TABLE t6 ALTER x TYPE int USING y;",
            "column \"y\" does not exist",
        ),
        (
            "CREATE TABLE t6 (x int); ALTER TABLE t6 ALTER x TYPE int COLLATE \"C\";",
            "collations are not supported by type integer",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a text); ALTER TABLE t ALTER COLUMN a TYPE int USING a::int;
         CREATE TABLE t5 (x int DEFAULT 1); ALTER TABLE t5 ALTER x TYPE bigint;
         CREATE TABLE t6 (x int); ALTER TABLE t6 ALTER x TYPE date USING now();",
    )]);
}

#[test]
fn alter_column_type_sets_the_collation() {
    // PG 18: COLLATE "C" is applied; without COLLATE the new type's default
    // replaces the old explicit collation.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t2 (a varchar(5)); ALTER TABLE t2 ALTER COLUMN a TYPE text COLLATE \"C\";
         CREATE TABLE t3 (a text COLLATE \"C\"); ALTER TABLE t3 ALTER a TYPE varchar(3);",
    )]);
    assert_cols(
        &db.analyze("SELECT a FROM t2").unwrap(),
        vec![cn("a", basic_with_collation("pg_catalog", "text", "C"))],
    );
    assert_cols(
        &db.analyze("SELECT a FROM t3").unwrap(),
        vec![cn("a", varchar_n(3))],
    );
}

#[test]
fn collate_on_a_non_collatable_column_is_rejected() {
    // PG 18: 42804 collations are not supported by type integer.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE t (a int COLLATE \"C\");")]),
        DdlError::Parse(_),
        "collations are not supported by type integer",
    );
}

// ── DROP / SET EXPRESSION, relation lookups, renames ────────────────────────

#[test]
fn drop_expression_makes_the_column_writable() {
    // PG 18: after DROP EXPRESSION `INSERT INTO t (a, b)` succeeds.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a * 2) STORED);
         ALTER TABLE t ALTER COLUMN b DROP EXPRESSION;
         ALTER TABLE t ALTER COLUMN a DROP EXPRESSION IF EXISTS;",
    )]);
    db.analyze("INSERT INTO t (a, b) VALUES (1, 2)").unwrap();
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (a int); ALTER TABLE t ALTER COLUMN a DROP EXPRESSION;",
        )]),
        DdlError::Parse(_),
        "column \"a\" of relation \"t\" is not a generated column",
    );
}

#[test]
fn set_expression_checks_the_column_and_expression() {
    build_db(&[(
        "0001.sql",
        "CREATE TABLE u (a int, b int GENERATED ALWAYS AS (a * 2) STORED);
         ALTER TABLE u ALTER COLUMN b SET EXPRESSION AS (a * 3);",
    )]);
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE u (a int); ALTER TABLE u ALTER COLUMN a SET EXPRESSION AS (1);",
        )]),
        DdlError::Parse(_),
        "column \"a\" of relation \"u\" is not a generated column",
    );
    let err = try_apply(&[(
        "0001.sql",
        "CREATE TABLE u (a int, b int GENERATED ALWAYS AS (a * 2) STORED);
         ALTER TABLE u ALTER COLUMN b SET EXPRESSION AS ('x');",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("invalid input syntax for type integer: \"x\""),
        "{err}"
    );
}

#[test]
fn alter_index_rename_is_tracked() {
    // PG 18: after RENAME the index is known under its new name.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE i (a int);
         CREATE INDEX ix ON i (a);
         ALTER INDEX ix RENAME TO jx;
         ALTER INDEX jx SET (fillfactor = 50);
         DROP INDEX jx;
         ALTER INDEX IF EXISTS nosuch RENAME TO z;
         CREATE TABLE k (a int PRIMARY KEY);
         ALTER INDEX k_pkey RENAME TO k_pk;
         ALTER TABLE k DROP CONSTRAINT k_pk;",
    )]);
    assert_ddl_err!(
        try_apply(&[("0001.sql", "ALTER INDEX nosuch RENAME TO z;")]),
        DdlError::TableNotFound(_),
        "relation \"nosuch\" does not exist",
    );
}

#[test]
fn missing_relations_are_reported_like_pg() {
    // PG 18: 42P01 relation "nosuch" does not exist / 3F000 schema "nosuch"
    // does not exist.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "ALTER TABLE nosuch ADD COLUMN a int;")]),
        DdlError::TableNotFound(_),
        "relation \"nosuch\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[("0001.sql", "ALTER TABLE nosuch.t ADD COLUMN a int;")]),
        DdlError::TableNotFound(_),
        "schema \"nosuch\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[("0001.sql", "ALTER INDEX nosuch2 SET (fillfactor = 50);")]),
        DdlError::TableNotFound(_),
        "relation \"nosuch2\" does not exist",
    );
}

#[test]
fn rename_to_a_taken_name_is_rejected() {
    // PG 18: 42P07 relation "b" already exists.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE a (x int); CREATE TABLE b (x int); ALTER TABLE a RENAME TO b;",
        )]),
        DdlError::DuplicateObject(_),
        "relation \"b\" already exists",
    );
    // RENAME COLUMN to a taken name / of a missing column.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE a (x int, y int); ALTER TABLE a RENAME COLUMN x TO y;",
        )]),
        DdlError::DuplicateObject(_),
        "column \"y\" of relation \"a\" already exists",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE a (x int); ALTER TABLE a RENAME COLUMN q TO z;",
        )]),
        DdlError::Parse(_),
        "column \"q\" does not exist",
    );
}

// ── ALTER TABLE subcommands per relation kind (ATSimplePermissions) ─────────

#[test]
fn alter_table_actions_are_limited_to_their_relation_kinds() {
    let setup = "CREATE TABLE t (a int);
                 CREATE VIEW v AS SELECT a FROM t;
                 CREATE MATERIALIZED VIEW mv AS SELECT a FROM t;
                 CREATE SEQUENCE s;
                 CREATE TYPE c AS (x int);";
    for (stmt, msg) in [
        (
            "ALTER TABLE v ADD COLUMN b int;",
            "ALTER action ADD COLUMN cannot be performed on relation \"v\"",
        ),
        (
            "ALTER TABLE v DROP COLUMN a;",
            "ALTER action DROP COLUMN cannot be performed on relation \"v\"",
        ),
        (
            "ALTER TABLE v ALTER COLUMN a TYPE bigint;",
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"v\"",
        ),
        (
            "ALTER TABLE v ALTER a SET NOT NULL;",
            "ALTER action ALTER COLUMN ... SET NOT NULL cannot be performed on relation \"v\"",
        ),
        (
            "ALTER TABLE v ADD CONSTRAINT k CHECK (a > 0);",
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"v\"",
        ),
        (
            "ALTER TABLE mv ADD COLUMN b int;",
            "ALTER action ADD COLUMN cannot be performed on relation \"mv\"",
        ),
        (
            "ALTER TABLE mv ALTER COLUMN a SET DEFAULT 1;",
            "ALTER action ALTER COLUMN ... SET DEFAULT cannot be performed on relation \"mv\"",
        ),
        (
            "ALTER TABLE s ADD COLUMN b int;",
            "ALTER action ADD COLUMN cannot be performed on relation \"s\"",
        ),
        (
            "ALTER TABLE c ADD COLUMN y int;",
            "\"c\" is a composite type",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    // What those relations do accept.
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE v ALTER COLUMN a SET DEFAULT 1;
             ALTER TYPE c ADD ATTRIBUTE y int;",
        ),
    ]);
}

#[test]
fn partition_key_columns_can_be_neither_dropped_nor_retyped() {
    // has_partition_attrs covers key columns and columns read by a key
    // expression (PG 18 wording).
    for (setup, stmt, msg) in [
        (
            "CREATE TABLE pp (a int, b int) PARTITION BY LIST (a);",
            "ALTER TABLE pp DROP COLUMN a;",
            "cannot drop column \"a\" because it is part of the partition key of relation \"pp\"",
        ),
        (
            "CREATE TABLE pe (a int, b int) PARTITION BY LIST ((a + b));",
            "ALTER TABLE pe DROP COLUMN b;",
            "cannot drop column \"b\" because it is part of the partition key of relation \"pe\"",
        ),
        (
            "CREATE TABLE pe (a int, b int) PARTITION BY LIST ((a + b));",
            "ALTER TABLE pe ALTER COLUMN b TYPE bigint;",
            "cannot alter column \"b\" because it is part of the partition key of relation \"pe\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE pp (a int, b int) PARTITION BY LIST (a);
         ALTER TABLE pp DROP COLUMN b;
         ALTER TABLE pp ADD COLUMN c int;
         ALTER TABLE pp ALTER COLUMN c TYPE bigint;",
    )]);
}

#[test]
fn a_row_type_stored_elsewhere_pins_the_table() {
    // find_composite_type_dependencies (PG 18): rewriting a table — ALTER
    // COLUMN TYPE, or ADD COLUMN with a default — or retyping a virtual or
    // partitioned table's column is refused while a stored column holds its
    // row type, directly or through an array / domain.
    let setup = "CREATE TABLE t1 (a int, v text GENERATED ALWAYS AS ('hello') VIRTUAL);
                 CREATE TABLE t2 (x t1);
                 CREATE TABLE t3 (a int);
                 CREATE TABLE t4 (x t3[]);
                 CREATE TABLE p (a int, b int) PARTITION BY LIST (a);
                 CREATE TABLE pu (x p);
                 CREATE TYPE ct AS (x int);
                 CREATE TABLE ut (c ct);";
    for (stmt, msg) in [
        (
            "ALTER TABLE t1 ALTER COLUMN v TYPE varchar;",
            "cannot alter table \"t1\" because column \"t2.x\" uses its row type",
        ),
        (
            "ALTER TABLE t1 ADD COLUMN c int DEFAULT 1;",
            "cannot alter table \"t1\" because column \"t2.x\" uses its row type",
        ),
        (
            "ALTER TABLE t1 ADD COLUMN c serial;",
            "cannot alter table \"t1\" because column \"t2.x\" uses its row type",
        ),
        (
            "ALTER TABLE t3 ALTER COLUMN a TYPE bigint;",
            "cannot alter table \"t3\" because column \"t4.x\" uses its row type",
        ),
        (
            "ALTER TABLE p ALTER COLUMN b TYPE bigint;",
            "cannot alter table \"p\" because column \"pu.x\" uses its row type",
        ),
        (
            "ALTER TYPE ct ALTER ATTRIBUTE x TYPE bigint;",
            "cannot alter type \"ct\" because column \"ut.c\" uses it",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE t1 ADD COLUMN c int;
             ALTER TABLE t1 ALTER COLUMN c SET NOT NULL;
             ALTER TABLE p ADD COLUMN d int DEFAULT 1;
             ALTER TYPE ct ADD ATTRIBUTE y int;
             ALTER TYPE ct DROP ATTRIBUTE y;",
        ),
    ]);
}

#[test]
fn set_logged_applies_to_tables_and_sequences_only() {
    // ATSimplePermissions(ATT_TABLE | ATT_SEQUENCE): PG 18 refuses it on a
    // partitioned table.
    let setup = "CREATE TABLE p (a int) PARTITION BY LIST (a);
                 CREATE VIEW v AS SELECT 1 AS a;
                 CREATE SEQUENCE s;";
    for (stmt, msg) in [
        (
            "ALTER TABLE p SET UNLOGGED;",
            "ALTER action SET UNLOGGED cannot be performed on relation \"p\" (This operation is \
             not supported for partitioned tables.)",
        ),
        (
            "ALTER TABLE p SET LOGGED;",
            "ALTER action SET LOGGED cannot be performed on relation \"p\"",
        ),
        (
            "ALTER TABLE v SET LOGGED;",
            "ALTER action SET LOGGED cannot be performed on relation \"v\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE s SET UNLOGGED; ALTER TABLE s SET LOGGED;",
        ),
    ]);
}

#[test]
fn alter_column_type_using_rejects_what_transform_expressions_forbid() {
    // EXPR_KIND_ALTER_COL_TRANSFORM: no set-returning function, aggregate,
    // window function or sub-select in the USING expression.
    for (sql, message) in [
        (
            "ALTER TABLE t ALTER COLUMN a TYPE bigint USING generate_series(1, a);",
            "set-returning functions are not allowed in transform expressions",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a TYPE bigint USING sum(a);",
            "aggregate functions are not allowed in transform expressions",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a TYPE bigint USING row_number() OVER ();",
            "window functions are not allowed in transform expressions",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a TYPE bigint USING (SELECT 1);",
            "cannot use subquery in transform expression",
        ),
    ] {
        let err = try_apply(&[("0001.sql", "CREATE TABLE t (a int);"), ("0002.sql", sql)])
            .expect_err(sql);
        assert!(err.to_string().starts_with(message), "{sql}\n  got: {err}");
    }
}

#[test]
fn a_column_may_not_hold_its_own_tables_row_type() {
    // CheckAttributeType within the relation's row type: directly, through
    // an array or through another composite that contains it.
    let setup = "CREATE TABLE a (x int, w int);
                 CREATE TABLE b (y a);
                 CREATE TYPE ct AS (x int);";
    for stmt in [
        "ALTER TABLE a ADD COLUMN y a;",
        "ALTER TABLE a ADD COLUMN y a[];",
        "ALTER TABLE a ADD COLUMN z b;",
        "ALTER TABLE a ALTER COLUMN w TYPE b USING NULL;",
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(
            err.to_string()
                .starts_with("composite type a cannot be made a member of itself"),
            "{stmt}\n  got: {err}"
        );
    }
    let stmt = "ALTER TYPE ct ADD ATTRIBUTE s ct;";
    let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
    assert!(
        err.to_string()
            .starts_with("composite type ct cannot be made a member of itself"),
        "got: {err}"
    );
}

#[test]
fn a_partition_gets_no_column_of_its_own() {
    // ATExecAddColumn: a partition's columns are its parent's; a plain
    // inheritance child may add some.
    let setup = "CREATE TABLE p (a int) PARTITION BY RANGE (a);
                 CREATE TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);
                 CREATE TABLE q (a int);
                 CREATE TABLE qc () INHERITS (q);";
    let stmt = "ALTER TABLE c ADD COLUMN b int;";
    let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
    assert!(
        err.to_string()
            .starts_with("cannot add column to a partition"),
        "got: {err}"
    );
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE qc ADD COLUMN b int; ALTER TABLE p ADD COLUMN b int;",
        ),
    ]);
}

#[test]
fn toast_storage_parameters_are_checked_only_with_a_toast_table() {
    // ATExecSetRelOptions validates `toast.` options against the TOAST
    // table, which exists once a CREATE / ALTER left the relation with a
    // column needing one (needs_toast_table) and survives the column;
    // CREATE TABLE ... WITH always validates them.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE a (x int);
         ALTER TABLE a SET (toast.fillfactor = 50);
         ALTER TABLE a SET (toast.fillfactor = 5, toast.toast_tuple_target = 200);
         CREATE TABLE c (x int);
         ALTER TABLE c ADD COLUMN t text, SET (toast.fillfactor = 50);
         CREATE TABLE d (v varchar(500));
         ALTER TABLE d SET (toast.fillfactor = 50);
         CREATE TABLE h (n numeric(1000, 0));
         ALTER TABLE h SET (toast.fillfactor = 50);
         CREATE TABLE f (t text STORAGE plain);
         ALTER TABLE f SET (toast.fillfactor = 50);
         CREATE TABLE fc () INHERITS (f);
         ALTER TABLE fc SET (toast.fillfactor = 50);
         CREATE MATERIALIZED VIEW m AS SELECT 1 AS x;
         ALTER MATERIALIZED VIEW m SET (toast.fillfactor = 50);",
    )]);
    let setup = "CREATE TABLE a (x int, t text);
                 CREATE TABLE b (x int, t text);
                 ALTER TABLE b DROP COLUMN t;
                 CREATE TABLE c (x int);
                 ALTER TABLE c ADD COLUMN t text;
                 CREATE TABLE e (v varchar(600));
                 CREATE TABLE k (v varchar(500), n numeric(1000, 0));
                 CREATE TABLE g (t text);
                 ALTER TABLE g ALTER COLUMN t SET STORAGE plain;
                 CREATE MATERIALIZED VIEW m AS SELECT 'x'::text AS x;";
    for stmt in [
        "ALTER TABLE a SET (toast.fillfactor = 50);",
        "ALTER TABLE b SET (toast.fillfactor = 50);",
        "ALTER TABLE c SET (toast.fillfactor = 50);",
        "ALTER TABLE e SET (toast.fillfactor = 50);",
        "ALTER TABLE k SET (toast.fillfactor = 50);",
        "ALTER TABLE g SET (toast.fillfactor = 50);",
        "ALTER MATERIALIZED VIEW m SET (toast.fillfactor = 50);",
        "CREATE TABLE n (x int) WITH (toast.fillfactor = 50);",
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(
            err.to_string()
                .starts_with("unrecognized parameter \"fillfactor\""),
            "{stmt}\n  got: {err}"
        );
    }
}

#[test]
fn user_relations_stay_out_of_pg_global() {
    // DefineRelation / DefineIndex / ATExecSetTableSpace and
    // AlterTableMoveAll: pg_global holds only shared catalogs.
    let setup = "CREATE TABLE a (x int);
                 CREATE INDEX i ON a (x);
                 CREATE MATERIALIZED VIEW m AS SELECT 1 AS x;
                 CREATE SEQUENCE s;";
    let placed = "only shared relations can be placed in pg_global tablespace";
    for (stmt, msg) in [
        ("ALTER TABLE a SET TABLESPACE pg_global;", placed),
        ("ALTER INDEX i SET TABLESPACE pg_global;", placed),
        (
            "ALTER MATERIALIZED VIEW m SET TABLESPACE pg_global;",
            placed,
        ),
        ("CREATE TABLE b (x int) TABLESPACE pg_global;", placed),
        (
            "CREATE TABLE p (x int) PARTITION BY RANGE (x) TABLESPACE pg_global;",
            placed,
        ),
        (
            "CREATE TABLE c TABLESPACE pg_global AS SELECT 1 AS x;",
            placed,
        ),
        ("CREATE INDEX j ON a (x) TABLESPACE pg_global;", placed),
        (
            "CREATE TABLE u (x int, UNIQUE (x) USING INDEX TABLESPACE pg_global);",
            placed,
        ),
        (
            "ALTER TABLE a ADD PRIMARY KEY (x) USING INDEX TABLESPACE pg_global;",
            placed,
        ),
        (
            "ALTER TABLE a SET TABLESPACE pg_default, SET TABLESPACE pg_default;",
            "cannot have multiple SET TABLESPACE subcommands",
        ),
        (
            "ALTER SEQUENCE s SET TABLESPACE pg_default;",
            "ALTER action SET TABLESPACE cannot be performed on relation \"s\"",
        ),
        (
            "ALTER TABLE ALL IN TABLESPACE pg_default SET TABLESPACE pg_global;",
            "cannot move relations in to or out of pg_global tablespace",
        ),
        (
            "ALTER INDEX ALL IN TABLESPACE pg_global SET TABLESPACE pg_default;",
            "cannot move relations in to or out of pg_global tablespace",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE a SET TABLESPACE pg_default;
             ALTER INDEX i SET TABLESPACE pg_default;
             ALTER TABLE ALL IN TABLESPACE pg_default SET TABLESPACE pg_default;
             ALTER MATERIALIZED VIEW ALL IN TABLESPACE pg_default
                 SET TABLESPACE pg_default NOWAIT;
             ALTER TABLE ALL IN TABLESPACE pg_default OWNED BY current_user
                 SET TABLESPACE pg_default;",
        ),
    ]);
}

#[test]
fn persistence_changes_follow_at_prep_change_persistence() {
    // A temporary relation's persistence can't change; the current one is
    // a no-op; it changes once per statement; a table listed in a
    // publication can't become unlogged.
    let setup = "CREATE TEMP TABLE a (x int);
                 CREATE TEMP SEQUENCE s;
                 CREATE UNLOGGED TABLE u (x int);
                 CREATE TABLE p (x int);
                 CREATE PUBLICATION pub FOR TABLE p;";
    for (stmt, msg) in [
        (
            "ALTER TABLE a SET UNLOGGED;",
            "cannot change logged status of table \"a\" because it is temporary",
        ),
        (
            "ALTER TABLE a SET LOGGED;",
            "cannot change logged status of table \"a\" because it is temporary",
        ),
        (
            "ALTER SEQUENCE s SET LOGGED;",
            "cannot change logged status of table \"s\" because it is temporary",
        ),
        (
            "ALTER TABLE u SET LOGGED, SET LOGGED;",
            "cannot change persistence setting twice",
        ),
        (
            "ALTER TABLE p SET UNLOGGED;",
            "cannot change table \"p\" to unlogged because it is part of a publication",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE p SET LOGGED;
             CREATE TABLE q (x int);
             ALTER TABLE q SET LOGGED, SET UNLOGGED;",
        ),
    ]);
}

#[test]
fn attach_partition_keeps_persistence_consistent() {
    // ATExecAttachPartition: a permanent table's partitions are permanent,
    // a temporary one's temporary.
    let setup = "CREATE TABLE p (a int) PARTITION BY RANGE (a);
                 CREATE TEMP TABLE c (a int);
                 CREATE TEMP TABLE tp (a int) PARTITION BY RANGE (a);
                 CREATE TABLE d (a int);";
    for (stmt, msg) in [
        (
            "ALTER TABLE p ATTACH PARTITION c FOR VALUES FROM (1) TO (2);",
            "cannot attach a temporary relation as partition of permanent relation \"p\"",
        ),
        (
            "ALTER TABLE tp ATTACH PARTITION d FOR VALUES FROM (1) TO (2);",
            "cannot attach a permanent relation as partition of temporary relation \"tp\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE p ATTACH PARTITION d FOR VALUES FROM (1) TO (2);
             ALTER TABLE tp ATTACH PARTITION c FOR VALUES FROM (1) TO (2);",
        ),
    ]);
}

#[test]
fn detach_concurrently_and_finalize() {
    // ATExecDetachPartition refuses a concurrent detach while a default
    // partition exists; a concurrent detach always completes in a
    // migration, so FINALIZE finds nothing pending; the detached partition
    // keeps its partition constraint as a CHECK (none for a hash one).
    let setup = "CREATE TABLE p (a int) PARTITION BY RANGE (a);
                 CREATE TABLE c PARTITION OF p DEFAULT;
                 CREATE TABLE c2 PARTITION OF p FOR VALUES FROM (1) TO (2);
                 CREATE TABLE x (a int);";
    for (stmt, msg) in [
        (
            "-- no-transaction\nALTER TABLE p DETACH PARTITION c CONCURRENTLY",
            "cannot detach partitions concurrently when a default partition exists",
        ),
        (
            "-- no-transaction\nALTER TABLE p DETACH PARTITION c2 CONCURRENTLY",
            "cannot detach partitions concurrently when a default partition exists",
        ),
        (
            "ALTER TABLE p DETACH PARTITION c2 FINALIZE;",
            "cannot complete detaching partition \"c2\"",
        ),
        (
            "ALTER TABLE p DETACH PARTITION x FINALIZE;",
            "relation \"x\" is not a partition of relation \"p\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        (
            "0001.sql",
            "CREATE TABLE q (a int, b text) PARTITION BY RANGE (a);
             CREATE TABLE q1 PARTITION OF q FOR VALUES FROM (1) TO (2);
             CREATE TABLE r (a int, b int) PARTITION BY RANGE (a, b);
             CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (1, MINVALUE) TO (2, 5);
             CREATE TABLE l (a int) PARTITION BY LIST (a);
             CREATE TABLE l1 PARTITION OF l FOR VALUES IN (NULL);
             CREATE TABLE h (a int) PARTITION BY HASH (a);
             CREATE TABLE h1 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 0);",
        ),
        (
            "0002.sql",
            "-- no-transaction\nALTER TABLE q DETACH PARTITION q1 CONCURRENTLY",
        ),
        (
            "0003.sql",
            "-- no-transaction\nALTER TABLE r DETACH PARTITION r1 CONCURRENTLY",
        ),
        (
            "0004.sql",
            "-- no-transaction\nALTER TABLE l DETACH PARTITION l1 CONCURRENTLY",
        ),
        (
            "0005.sql",
            "-- no-transaction\nALTER TABLE h DETACH PARTITION h1 CONCURRENTLY",
        ),
        (
            "0006.sql",
            "ALTER TABLE q1 DROP CONSTRAINT q1_a_check;
             ALTER TABLE r1 DROP CONSTRAINT r1_check;
             ALTER TABLE l1 DROP CONSTRAINT l1_a_check;
             ALTER TABLE h1 ADD CONSTRAINT h1_a_check CHECK (a > 0);",
        ),
    ]);
}

#[test]
fn system_columns_can_be_neither_dropped_nor_altered() {
    // A relation with storage has system columns in pg_attribute: DROP
    // COLUMN finds them (IF EXISTS doesn't skip) and refuses; ALTER COLUMN
    // subcommands say "cannot alter system column"; a not-null constraint
    // or an index key on one is refused. Views and composite types have
    // none.
    let setup = "CREATE TABLE t (a int);
                 CREATE TABLE p (a int) PARTITION BY RANGE (a);
                 CREATE TYPE ct AS (x int);
                 CREATE VIEW v AS SELECT 1 AS a;";
    for (stmt, msg) in [
        (
            "ALTER TABLE t DROP COLUMN ctid;",
            "cannot drop system column \"ctid\"",
        ),
        (
            "ALTER TABLE t DROP COLUMN IF EXISTS xmin;",
            "cannot drop system column \"xmin\"",
        ),
        (
            "ALTER TABLE p DROP COLUMN tableoid;",
            "cannot drop system column \"tableoid\"",
        ),
        (
            "CREATE TABLE t2 (a int, NOT NULL ctid);",
            "cannot add not-null constraint on system column \"ctid\"",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT n NOT NULL ctid;",
            "cannot add not-null constraint on system column \"ctid\"",
        ),
        (
            "ALTER TABLE t ADD PRIMARY KEY (ctid);",
            "cannot add not-null constraint on system column \"ctid\"",
        ),
        (
            "CREATE TABLE t4 (a int, PRIMARY KEY (ctid));",
            "index creation on system columns is not supported",
        ),
        (
            "ALTER TABLE t ADD UNIQUE (a, ctid);",
            "index creation on system columns is not supported",
        ),
        (
            "ALTER TABLE t ALTER COLUMN ctid SET DEFAULT 1;",
            "cannot alter system column \"ctid\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN ctid DROP NOT NULL;",
            "cannot alter system column \"ctid\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN xmin TYPE text;",
            "cannot alter system column \"xmin\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN xmin DROP IDENTITY;",
            "cannot alter system column \"xmin\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN xmin SET GENERATED ALWAYS;",
            "cannot alter system column \"xmin\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN xmin ADD GENERATED ALWAYS AS IDENTITY;",
            "identity column type must be smallint, integer, or bigint",
        ),
        (
            "ALTER TYPE ct DROP ATTRIBUTE ctid;",
            "column \"ctid\" of relation \"ct\" does not exist",
        ),
        (
            "ALTER VIEW v ALTER COLUMN ctid SET DEFAULT 1;",
            "column \"ctid\" of relation \"v\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
}

#[test]
fn alter_column_type_of_an_identity_column_retypes_its_sequence() {
    // transformAlterTableStmt runs ALTER SEQUENCE ... AS <new type> on the
    // identity sequence first: the type must be an integer one, and the
    // sequence's bounds follow (not for a partition, whose sequence is its
    // parent's).
    let setup = "CREATE TABLE t (a int GENERATED ALWAYS AS IDENTITY);
                 CREATE TABLE u (a bigint GENERATED ALWAYS AS IDENTITY (MAXVALUE 100000));
                 CREATE TABLE p (a int GENERATED ALWAYS AS IDENTITY, b int)
                     PARTITION BY RANGE (b);
                 CREATE TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);";
    for (stmt, msg) in [
        (
            "ALTER TABLE t ALTER COLUMN a TYPE text;",
            "identity column type must be smallint, integer, or bigint",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a TYPE numeric;",
            "identity column type must be smallint, integer, or bigint",
        ),
        (
            "ALTER TABLE p ALTER COLUMN a TYPE text;",
            "identity column type must be smallint, integer, or bigint",
        ),
        (
            "ALTER TABLE u ALTER COLUMN a TYPE smallint;",
            "MAXVALUE (100000) is out of range for sequence data type smallint",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a TYPE smallint;
             ALTER TABLE t ALTER COLUMN a RESTART WITH 40000;",
            "RESTART value (40000) cannot be greater than MAXVALUE (32767)",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN a TYPE bigint;
             ALTER TABLE p ALTER COLUMN a TYPE bigint;",
        ),
    ]);
}

#[test]
fn a_partitioned_tables_identity_changes_through_the_whole_tree() {
    // ATExecAddIdentity / SetIdentity / DropIdentity: the partitions share
    // the parent's identity — it changes neither under ONLY nor on a
    // partition, and reaches every partition (ATTACH gives it to the new
    // one, DETACH takes it away). A regular inheritance child has its own.
    let setup = "CREATE TABLE p (a int GENERATED ALWAYS AS IDENTITY, b int)
                     PARTITION BY RANGE (b);
                 CREATE TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);
                 CREATE TABLE r (a int GENERATED ALWAYS AS IDENTITY);
                 CREATE TABLE rc () INHERITS (r);";
    for (stmt, msg) in [
        (
            "ALTER TABLE c ALTER COLUMN a DROP IDENTITY;",
            "cannot drop identity from a column of a partition",
        ),
        (
            "ALTER TABLE c ALTER COLUMN a SET GENERATED BY DEFAULT;",
            "cannot change identity column of a partition",
        ),
        (
            "ALTER TABLE c ALTER COLUMN a RESTART;",
            "cannot change identity column of a partition",
        ),
        (
            "ALTER TABLE ONLY p ALTER COLUMN a DROP IDENTITY;",
            "cannot drop identity from a column of only the partitioned table",
        ),
        (
            "ALTER TABLE ONLY p ALTER COLUMN a SET GENERATED BY DEFAULT;",
            "cannot change identity column of only the partitioned table",
        ),
        (
            "ALTER TABLE p ALTER COLUMN a DROP IDENTITY;
             ALTER TABLE ONLY p ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY;",
            "cannot add identity to a column of only the partitioned table",
        ),
        (
            "ALTER TABLE p ALTER COLUMN a DROP IDENTITY;
             ALTER TABLE c ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY;",
            "cannot add identity to a column of a partition",
        ),
        (
            "CREATE TABLE d (a int NOT NULL, b int);
             ALTER TABLE p ATTACH PARTITION d FOR VALUES FROM (2) TO (3);
             ALTER TABLE d ALTER COLUMN a SET DEFAULT 1;",
            "column \"a\" of relation \"d\" is an identity column",
        ),
        (
            "ALTER TABLE ONLY r ALTER COLUMN a DROP IDENTITY;
             ALTER TABLE rc ALTER COLUMN a DROP IDENTITY;",
            "column \"a\" of relation \"rc\" is not an identity column",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE p ALTER COLUMN a DROP IDENTITY;
             ALTER TABLE p ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY;
             CREATE TABLE d (a int NOT NULL, b int);
             ALTER TABLE p ATTACH PARTITION d FOR VALUES FROM (2) TO (3);
             ALTER TABLE p ALTER COLUMN a SET GENERATED BY DEFAULT;
             ALTER TABLE p DETACH PARTITION c;
             ALTER TABLE c ALTER COLUMN a SET DEFAULT 1;",
        ),
    ]);
}

#[test]
fn drop_column_only_refuses_a_partitioned_table_with_partitions() {
    // ATExecDropColumn: a partition's columns are its parent's, so ONLY is
    // refused while partitions exist (a regular parent's children keep the
    // column as their own).
    let setup = "CREATE TABLE p (a int, b int) PARTITION BY RANGE (a);
                 CREATE TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);
                 CREATE TABLE e (a int, b int) PARTITION BY RANGE (a);
                 CREATE TABLE r (a int, b int);
                 CREATE TABLE rc () INHERITS (r);";
    let stmt = "ALTER TABLE ONLY p DROP COLUMN b;";
    let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
    assert!(
        err.to_string().starts_with(
            "cannot drop column from only the partitioned table when partitions exist"
        ),
        "got: {err}"
    );
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE p DROP COLUMN b;
             ALTER TABLE ONLY e DROP COLUMN b;
             ALTER TABLE ONLY r DROP COLUMN b;",
        ),
    ]);
    db.analyze("SELECT * FROM c").unwrap();
    db.analyze("SELECT b FROM rc").unwrap();
}

#[test]
fn alter_table_subcommands_run_pass_by_pass() {
    // ATController runs the subcommands by pass: DROPs, then ALTER TYPE,
    // then ADD COLUMN, then constraints and defaults. A column's type
    // changes at most once per statement.
    let setup = "CREATE TABLE t (a int, b int);";
    for (stmt, msg) in [
        (
            "ALTER TABLE t ALTER COLUMN a TYPE text, ALTER COLUMN a TYPE bigint;",
            "cannot alter type of column \"a\" twice",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a TYPE text, DROP COLUMN a;",
            "column \"a\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t ADD COLUMN c int, ALTER COLUMN c TYPE text;",
            "column \"c\" of relation \"t\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN a TYPE int, ALTER COLUMN a TYPE bigint;
             ALTER TABLE t ALTER COLUMN b SET DEFAULT 'x', ALTER COLUMN b TYPE text;
             ALTER TABLE t DROP COLUMN b, ADD COLUMN b text;
             ALTER TABLE t ADD COLUMN d int, ADD CONSTRAINT dc CHECK (d > 0),
                 ALTER COLUMN d SET NOT NULL;",
        ),
    ]);
}

#[test]
fn alter_column_type_rebuilds_what_reads_the_column() {
    // ATPostAlterTypeCleanup re-adds a partial index's predicate, the CHECK
    // constraints and the foreign keys over a retyped column from their
    // definitions: they must fit the new type.
    for (setup, stmt, msg) in [
        (
            "CREATE TABLE t (a int CHECK (a > 0));",
            "ALTER TABLE t ALTER COLUMN a TYPE text;",
            "operator does not exist: text > integer",
        ),
        (
            "CREATE TABLE t (a int); CREATE INDEX ON t ((a + 1));",
            "ALTER TABLE t ALTER COLUMN a TYPE text;",
            "operator does not exist: text + integer",
        ),
        (
            "CREATE TABLE t (a int); CREATE INDEX ON t (a) WHERE a > 0;",
            "ALTER TABLE t ALTER COLUMN a TYPE text;",
            "operator does not exist: text > integer",
        ),
        (
            "CREATE TABLE p (a int PRIMARY KEY); CREATE TABLE c (x int REFERENCES p);",
            "ALTER TABLE c ALTER COLUMN x TYPE text;",
            "foreign key constraint \"c_x_fkey\" cannot be implemented",
        ),
        (
            "CREATE TABLE p (a int PRIMARY KEY); CREATE TABLE c (x int REFERENCES p);",
            "ALTER TABLE p ALTER COLUMN a TYPE text;",
            "foreign key constraint \"c_x_fkey\" cannot be implemented",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        (
            "0001.sql",
            "CREATE TABLE p (a int PRIMARY KEY);
             CREATE TABLE c (x int REFERENCES p, y int CHECK (y > 0));
             CREATE INDEX ON c (y) WHERE y > 0;",
        ),
        (
            "0002.sql",
            "ALTER TABLE p ALTER COLUMN a TYPE bigint;
             ALTER TABLE c ALTER COLUMN x TYPE smallint;
             ALTER TABLE c ALTER COLUMN y TYPE bigint;",
        ),
    ]);
}

#[test]
fn policies_triggers_rules_and_sql_bodies_depend_on_what_they_read() {
    // CreatePolicy / CreateTrigger / InsertRule / ProcedureCreate record
    // pg_depend edges on the columns (and relations) they read: DROP
    // COLUMN / DROP TABLE of those needs CASCADE (which drops them), and
    // ALTER COLUMN TYPE is refused (RememberAllDependentForRebuilding).
    let trigger_fn = "CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS \
                      'BEGIN RETURN NEW; END';";
    let depends = "because other objects depend on it";
    for (setup, stmt, msg) in [
        (
            "CREATE POLICY p ON t USING (a > 0);",
            "ALTER TABLE t ALTER COLUMN a TYPE text;",
            "cannot alter type of a column used in a policy definition",
        ),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (new.a > 0)
                 EXECUTE FUNCTION tf();",
            "ALTER TABLE t ALTER COLUMN a TYPE text;",
            "cannot alter type of a column used in a trigger definition",
        ),
        (
            "CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO u VALUES (new.a);",
            "ALTER TABLE t ALTER COLUMN a TYPE bigint;",
            "cannot alter type of a column used by a view or rule",
        ),
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a FROM t; END;",
            "ALTER TABLE t ALTER COLUMN a TYPE bigint;",
            "cannot alter type of a column used by a function or procedure",
        ),
        (
            "CREATE POLICY p ON t USING (a > 0);",
            "ALTER TABLE t DROP COLUMN a;",
            depends,
        ),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (new.a > 0)
                 EXECUTE FUNCTION tf();",
            "ALTER TABLE t DROP COLUMN a;",
            depends,
        ),
        (
            "CREATE TRIGGER tr BEFORE UPDATE OF a ON t FOR EACH ROW EXECUTE FUNCTION tf();",
            "ALTER TABLE t DROP COLUMN a;",
            depends,
        ),
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a FROM t; END;",
            "ALTER TABLE t DROP COLUMN a;",
            depends,
        ),
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a FROM t; END;",
            "DROP TABLE t;",
            depends,
        ),
        (
            "CREATE POLICY p ON t USING (a IN (SELECT x FROM u));",
            "DROP TABLE u;",
            depends,
        ),
        (
            "CREATE POLICY p ON t USING (a > 0);
             ALTER POLICY p ON t USING (b > 0);
             ALTER POLICY p ON t RENAME TO q;",
            "ALTER TABLE t DROP COLUMN b;",
            "cannot drop column b of table t because other objects depend on it (policy q",
        ),
    ] {
        let base = format!("CREATE TABLE t (a int, b int); CREATE TABLE u (x int); {trigger_fn}");
        let err = try_apply(&[("0001.sql", &base), ("0002.sql", setup), ("0003.sql", stmt)])
            .expect_err(stmt);
        assert!(
            err.to_string().contains(msg),
            "{setup} / {stmt}\n  got: {err}"
        );
    }
    // CASCADE drops the dependents; an ALTER POLICY that stops reading a
    // column releases it.
    build_db(&[(
        "0001.sql",
        &format!(
            "CREATE TABLE t (a int, b int, c int); {trigger_fn}
             CREATE POLICY p ON t USING (a > 0);
             CREATE TRIGGER tr BEFORE UPDATE OF a ON t FOR EACH ROW EXECUTE FUNCTION tf();
             ALTER TABLE t DROP COLUMN a CASCADE;
             CREATE POLICY p ON t USING (b > 0);
             CREATE TRIGGER tr BEFORE UPDATE OF b ON t FOR EACH ROW EXECUTE FUNCTION tf();
             ALTER POLICY p ON t USING (true);
             DROP TRIGGER tr ON t;
             ALTER TABLE t DROP COLUMN b;
             CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT c FROM t; END;
             DROP TABLE t CASCADE;
             CREATE FUNCTION f() RETURNS int LANGUAGE sql RETURN 1;"
        ),
    )]);
}
