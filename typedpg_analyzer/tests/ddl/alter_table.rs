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
