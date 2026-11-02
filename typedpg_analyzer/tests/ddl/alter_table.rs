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

#[test]
fn alter_column_type_converts_only_stored_values() {
    // ATPrepAlterColumnType (PG 18): only a table or partitioned table has
    // values to convert. A composite type's attribute or a foreign table's
    // column changes type with no cast, and takes no USING.
    build_db(&[(
        "0001.sql",
        "CREATE TYPE test_type AS (a int, b text);
         ALTER TYPE test_type ALTER ATTRIBUTE b SET DATA TYPE integer,
                              ALTER ATTRIBUTE a TYPE point;
         CREATE FOREIGN DATA WRAPPER w; CREATE SERVER s FOREIGN DATA WRAPPER w;
         CREATE FOREIGN TABLE ft (a int, b text) SERVER s;
         ALTER FOREIGN TABLE ft ALTER COLUMN b TYPE int;",
    )]);
    let err = try_apply(&[(
        "0001.sql",
        "CREATE FOREIGN DATA WRAPPER w; CREATE SERVER s FOREIGN DATA WRAPPER w;
         CREATE FOREIGN TABLE ft (a int, b text) SERVER s;
         ALTER FOREIGN TABLE ft ALTER COLUMN a TYPE int USING a + 1;",
    )])
    .unwrap_err();
    assert!(
        err.to_string().starts_with("\"ft\" is not a table"),
        "{err}"
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
fn rename_column_of_a_missing_table_message() {
    // PG 18: RENAME COLUMN / CONSTRAINT look the relation up like every
    // other ALTER TABLE (RangeVarGetRelidExtended); IF EXISTS skips a
    // missing relation or schema.
    for (sql, msg) in [
        (
            "ALTER TABLE nonesuchrel RENAME COLUMN nonesuchatt TO newnonesuchatt;",
            "relation \"nonesuchrel\" does not exist",
        ),
        (
            "ALTER TABLE nosuch.t RENAME COLUMN a TO b;",
            "schema \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE nonesuchrel RENAME CONSTRAINT a TO b;",
            "relation \"nonesuchrel\" does not exist",
        ),
        (
            "ALTER TABLE nosuch.t RENAME CONSTRAINT a TO b;",
            "schema \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "ALTER TABLE IF EXISTS nonesuchrel RENAME COLUMN a TO b;
         ALTER TABLE IF EXISTS nosuch.t RENAME CONSTRAINT a TO b;",
    )]);
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
fn add_column_to_a_row_type_used_by_another_table() {
    // PG 18: ADD COLUMN rewrites the table — and so checks
    // find_composite_type_dependencies — only when the column gets a stored
    // default. A DEFAULT that cooks to a null constant gets none
    // (AddRelationNewConstraints skips it); a cast to another type, a
    // typmod's length coercion or a domain's CoerceToDomain is no constant.
    let setup = "CREATE DOMAIN d AS int;
                 CREATE TABLE fullname (first text, last text);
                 CREATE TABLE people (fn fullname, bd date);";
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE fullname ADD COLUMN suffix text DEFAULT NULL;",
        ),
    ]);
    db.analyze("SELECT (fn).suffix FROM people").unwrap();
    for stmt in [
        "ALTER TABLE fullname ADD COLUMN c text DEFAULT NULL::text::text;",
        "ALTER TABLE fullname ADD COLUMN c int DEFAULT CAST(NULL AS int);",
        "ALTER TABLE fullname ADD COLUMN c int DEFAULT NULL::unknown;",
        "ALTER TABLE fullname ADD COLUMN c int[] DEFAULT NULL;",
        "ALTER TABLE fullname ADD COLUMN c interval(0) DEFAULT NULL;",
    ] {
        build_db(&[("0001.sql", setup), ("0002.sql", stmt)]);
    }
    for stmt in [
        "ALTER TABLE fullname ADD COLUMN c text DEFAULT NULL::varchar;",
        "ALTER TABLE fullname ADD COLUMN c int DEFAULT NULL::int8;",
        "ALTER TABLE fullname ADD COLUMN c varchar(5) DEFAULT NULL;",
        "ALTER TABLE fullname ADD COLUMN c d DEFAULT NULL;",
        "ALTER TABLE fullname ADD COLUMN c int GENERATED ALWAYS AS (NULL) STORED;",
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(
            err.to_string().starts_with(
                "cannot alter table \"fullname\" because column \"people.fn\" uses its row type"
            ),
            "{stmt}\n  got: {err}"
        );
    }
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

// ── What ALTER TABLE plans ──────────────────────────────────────────────────

/// Apply `sql` and return its error message.
fn planned_err(sql: &str) -> String {
    match PgCatalog::new().unwrap().apply_sql(sql) {
        Ok(()) => panic!("expected {sql:?} to fail"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn alter_table_plans_what_it_evaluates_over_the_rows() {
    // ALTER TABLE evaluates a new CHECK, a new column's default and a
    // USING expression over the table's rows, planning them first: a
    // constant that can't fold fails the statement, rows or not.
    for (sql, msg) in [
        (
            "CREATE TABLE t (a int); ALTER TABLE t ADD CHECK (a < 40000::int2);",
            "smallint out of range",
        ),
        (
            "CREATE TABLE t (a int); ALTER TABLE t ADD COLUMN b int2 DEFAULT 40000;",
            "smallint out of range",
        ),
        (
            "CREATE TABLE t (a int); ALTER TABLE t ADD COLUMN b int DEFAULT 40000::int2;",
            "smallint out of range",
        ),
        (
            "CREATE TABLE t (a int); ALTER TABLE t ADD COLUMN b numeric(3,1) DEFAULT 100;",
            "numeric field overflow",
        ),
        (
            "CREATE TABLE t (a int); ALTER TABLE t ADD COLUMN b varchar(3) DEFAULT 'abcd';",
            "value too long for type character varying(3)",
        ),
        (
            "CREATE TABLE t (a int); ALTER TABLE t ALTER COLUMN a TYPE int8 USING a + 40000::int2;",
            "smallint out of range",
        ),
        (
            "CREATE TABLE t (a int); ALTER TABLE t ALTER COLUMN a TYPE int2 USING 40000;",
            "smallint out of range",
        ),
    ] {
        let err = planned_err(sql);
        assert!(
            err.starts_with(msg),
            "{sql}\n  expected: {msg}\n  got: {err}"
        );
    }
    // Not validated (NOT VALID, a partitioned table with no partition, a
    // CHECK created with its table), or only stored (a default for the
    // rows to come): PG doesn't plan them.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int CHECK (a < 40000::int2), b int2 DEFAULT 40000);
         ALTER TABLE t ADD CHECK (a < 40000::int2) NOT VALID;
         CREATE TABLE p (a int) PARTITION BY RANGE (a);
         ALTER TABLE p ADD CHECK (a < 40000::int2);",
    )]);
}

/// The relations [`RELKIND_CASES`] run over: one of each kind.
const RELKIND_SETUP: &str = "
    CREATE TABLE t (a int, b int); CREATE INDEX ti ON t (a);
    CREATE TABLE pt (a int, b int) PARTITION BY RANGE (a); CREATE INDEX pti ON pt (a);
    CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (1) TO (2);
    CREATE SEQUENCE s; CREATE VIEW v AS SELECT 1 AS a, 2 AS b;
    CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a, 2 AS b;
    CREATE FOREIGN DATA WRAPPER w; CREATE SERVER sv FOREIGN DATA WRAPPER w;
    CREATE FOREIGN TABLE ft (a int, b int) SERVER sv;
    CREATE TYPE ct AS (a int, b int);
    CREATE TABLE tt (a int, b int);
    CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';";

#[test]
fn alter_table_actions_and_relation_commands_follow_each_relation_kind() {
    // ATSimplePermissions (ATWrongRelkindError) for every ALTER TABLE
    // subcommand over every relation kind, and the relation kind checks of
    // REINDEX CONCURRENTLY (no system catalog), CREATE RULE, ATTACH /
    // DETACH PARTITION, PARTITION OF, USING INDEX and ALTER TRIGGER / RULE
    // ... RENAME — each as PostgreSQL 18.6 answers it. What PG runs is
    // tried in a transaction rolled back (but REINDEX CONCURRENTLY, which
    // can't run in one); what it refuses changes nothing.
    let mut db = build_db(&[("0001.sql", RELKIND_SETUP)]);
    for &(sql, expected) in RELKIND_CASES {
        match expected {
            None => {
                let wrapped = if sql.contains("concurrently") {
                    format!("{sql};")
                } else {
                    format!("BEGIN; {sql}; ROLLBACK;")
                };
                if let Err(err) = db.apply_sql(&wrapped) {
                    panic!("{sql}\n  rejected: {err}");
                }
            }
            Some(message) => {
                let err = db.apply_sql(&format!("{sql};")).expect_err(sql);
                assert!(
                    err.to_string().starts_with(message),
                    "{sql}\n  expected: {message}\n       got: {err}"
                );
            }
        }
    }
}

/// `(statement, PG's error)` — `None` when PG runs it — over the relations of
/// [`RELKIND_SETUP`], each checked on PostgreSQL 18.6.
const RELKIND_CASES: &[(&str, Option<&str>)] = &[
    ("alter table t add column c int", None),
    ("alter table t drop column b", None),
    ("alter table t alter column a set default 1", None),
    ("alter table t alter column a drop default", None),
    ("alter table t alter column a set not null", None),
    ("alter table t alter column a drop not null", None),
    ("alter table t alter column a type bigint", None),
    ("alter table t alter column a set statistics 5", None),
    ("alter table t alter column a set storage plain", None),
    (
        "alter table t alter column a set compression pglz",
        Some("column data type integer does not support compression"),
    ),
    ("alter table t alter column a set (n_distinct = 1)", None),
    ("alter table t alter column a reset (n_distinct)", None),
    (
        "alter table t alter column a add generated always as identity",
        Some(
            "column \"a\" of relation \"t\" must be declared NOT NULL before identity can be added",
        ),
    ),
    (
        "alter table t alter column a set generated always",
        Some("column \"a\" of relation \"t\" is not an identity column"),
    ),
    (
        "alter table t alter column a drop identity",
        Some("column \"a\" of relation \"t\" is not an identity column"),
    ),
    (
        "alter table t alter column a drop expression",
        Some("column \"a\" of relation \"t\" is not a generated column"),
    ),
    (
        "alter table t alter column a set expression as (1)",
        Some("column \"a\" of relation \"t\" is not a generated column"),
    ),
    ("alter table t add constraint k check (a > 0)", None),
    ("alter table t add primary key (a)", None),
    ("alter table t add unique (a)", None),
    (
        "alter table t add foreign key (a) references tt (a)",
        Some("there is no unique constraint matching given keys for referenced table \"tt\""),
    ),
    (
        "alter table t add constraint k check (a > 0) not valid",
        None,
    ),
    (
        "alter table t validate constraint k",
        Some("constraint \"k\" of relation \"t\" does not exist"),
    ),
    (
        "alter table t drop constraint k",
        Some("constraint \"k\" of relation \"t\" does not exist"),
    ),
    (
        "alter table t alter constraint k deferrable",
        Some("constraint \"k\" of relation \"t\" does not exist"),
    ),
    (
        "alter table t rename constraint k to k2",
        Some("constraint \"k\" for table \"t\" does not exist"),
    ),
    ("alter table t cluster on ti", None),
    ("alter table t set without cluster", None),
    ("alter table t set logged", None),
    ("alter table t set unlogged", None),
    ("alter table t set without oids", None),
    ("alter table t set access method heap", None),
    ("alter table t set tablespace pg_default", None),
    ("alter table t set (fillfactor = 50)", None),
    ("alter table t reset (fillfactor)", None),
    ("alter table t replica identity full", None),
    (
        "alter table t replica identity using index ti",
        Some("cannot use non-unique index \"ti\" as replica identity"),
    ),
    ("alter table t disable trigger all", None),
    ("alter table t enable trigger all", None),
    ("alter table t enable trigger user", None),
    (
        "alter table t enable always trigger tr",
        Some("trigger \"tr\" for table \"t\" does not exist"),
    ),
    (
        "alter table t disable trigger tr",
        Some("trigger \"tr\" for table \"t\" does not exist"),
    ),
    (
        "alter table t enable rule r",
        Some("rule \"r\" for relation \"t\" does not exist"),
    ),
    (
        "alter table t disable rule r",
        Some("rule \"r\" for relation \"t\" does not exist"),
    ),
    (
        "alter table t enable replica rule r",
        Some("rule \"r\" for relation \"t\" does not exist"),
    ),
    ("alter table t enable row level security", None),
    ("alter table t disable row level security", None),
    ("alter table t force row level security", None),
    ("alter table t no force row level security", None),
    ("alter table t inherit tt", None),
    (
        "alter table t no inherit tt",
        Some("relation \"tt\" is not a parent of relation \"t\""),
    ),
    ("alter table t of ct", None),
    ("alter table t not of", Some("\"t\" is not a typed table")),
    ("alter table t owner to postgres", None),
    (
        "alter table t attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"t\" (This operation is not supported for tables.",
        ),
    ),
    (
        "alter table t detach partition tt",
        Some(
            "ALTER action DETACH PARTITION cannot be performed on relation \"t\" (This operation is not supported for tables.",
        ),
    ),
    (
        "alter table t options (add x 'y')",
        Some(
            "ALTER action OPTIONS cannot be performed on relation \"t\" (This operation is not supported for tables.",
        ),
    ),
    (
        "alter table t alter column a options (add x 'y')",
        Some(
            "ALTER action ALTER COLUMN ... OPTIONS cannot be performed on relation \"t\" (This operation is not supported for tables.",
        ),
    ),
    ("alter table t rename to zz", None),
    ("alter table t rename column a to z", None),
    ("alter table t set schema public", None),
    ("alter table t alter column a set data type int", None),
    (
        "alter table t add column c int generated always as (a + 1) stored",
        None,
    ),
    ("alter table pt add column c int", None),
    ("alter table pt drop column b", None),
    ("alter table pt alter column a set default 1", None),
    ("alter table pt alter column a drop default", None),
    ("alter table pt alter column a set not null", None),
    ("alter table pt alter column a drop not null", None),
    (
        "alter table pt alter column a type bigint",
        Some(
            "cannot alter column \"a\" because it is part of the partition key of relation \"pt\"",
        ),
    ),
    ("alter table pt alter column a set statistics 5", None),
    ("alter table pt alter column a set storage plain", None),
    (
        "alter table pt alter column a set compression pglz",
        Some("column data type integer does not support compression"),
    ),
    ("alter table pt alter column a set (n_distinct = 1)", None),
    ("alter table pt alter column a reset (n_distinct)", None),
    (
        "alter table pt alter column a add generated always as identity",
        Some(
            "column \"a\" of relation \"pt\" must be declared NOT NULL before identity can be added",
        ),
    ),
    (
        "alter table pt alter column a set generated always",
        Some("column \"a\" of relation \"pt\" is not an identity column"),
    ),
    (
        "alter table pt alter column a drop identity",
        Some("column \"a\" of relation \"pt\" is not an identity column"),
    ),
    (
        "alter table pt alter column a drop expression",
        Some("column \"a\" of relation \"pt\" is not a generated column"),
    ),
    (
        "alter table pt alter column a set expression as (1)",
        Some("column \"a\" of relation \"pt\" is not a generated column"),
    ),
    ("alter table pt add constraint k check (a > 0)", None),
    ("alter table pt add primary key (a)", None),
    ("alter table pt add unique (a)", None),
    (
        "alter table pt add foreign key (a) references tt (a)",
        Some("there is no unique constraint matching given keys for referenced table \"tt\""),
    ),
    (
        "alter table pt add constraint k check (a > 0) not valid",
        None,
    ),
    (
        "alter table pt validate constraint k",
        Some("constraint \"k\" of relation \"pt\" does not exist"),
    ),
    (
        "alter table pt drop constraint k",
        Some("constraint \"k\" of relation \"pt\" does not exist"),
    ),
    (
        "alter table pt alter constraint k deferrable",
        Some("constraint \"k\" of relation \"pt\" does not exist"),
    ),
    (
        "alter table pt rename constraint k to k2",
        Some("constraint \"k\" for table \"pt\" does not exist"),
    ),
    (
        "alter table pt cluster on ti",
        Some("\"ti\" is not an index for table \"pt\""),
    ),
    (
        "alter table pt set without cluster",
        Some("cannot mark index clustered in partitioned table"),
    ),
    (
        "alter table pt set logged",
        Some(
            "ALTER action SET LOGGED cannot be performed on relation \"pt\" (This operation is not supported for partitioned tables.",
        ),
    ),
    (
        "alter table pt set unlogged",
        Some(
            "ALTER action SET UNLOGGED cannot be performed on relation \"pt\" (This operation is not supported for partitioned tables.",
        ),
    ),
    ("alter table pt set without oids", None),
    ("alter table pt set access method heap", None),
    ("alter table pt set tablespace pg_default", None),
    (
        "alter table pt set (fillfactor = 50)",
        Some("cannot specify storage parameters for a partitioned table"),
    ),
    ("alter table pt reset (fillfactor)", None),
    ("alter table pt replica identity full", None),
    (
        "alter table pt replica identity using index ti",
        Some("\"ti\" is not an index for table \"pt\""),
    ),
    ("alter table pt disable trigger all", None),
    ("alter table pt enable trigger all", None),
    ("alter table pt enable trigger user", None),
    (
        "alter table pt enable always trigger tr",
        Some("trigger \"tr\" for table \"pt\" does not exist"),
    ),
    (
        "alter table pt disable trigger tr",
        Some("trigger \"tr\" for table \"pt\" does not exist"),
    ),
    (
        "alter table pt enable rule r",
        Some("rule \"r\" for relation \"pt\" does not exist"),
    ),
    (
        "alter table pt disable rule r",
        Some("rule \"r\" for relation \"pt\" does not exist"),
    ),
    (
        "alter table pt enable replica rule r",
        Some("rule \"r\" for relation \"pt\" does not exist"),
    ),
    ("alter table pt enable row level security", None),
    ("alter table pt disable row level security", None),
    ("alter table pt force row level security", None),
    ("alter table pt no force row level security", None),
    (
        "alter table pt inherit tt",
        Some("cannot change inheritance of partitioned table"),
    ),
    (
        "alter table pt no inherit tt",
        Some("relation \"tt\" is not a parent of relation \"pt\""),
    ),
    ("alter table pt of ct", None),
    ("alter table pt not of", Some("\"pt\" is not a typed table")),
    ("alter table pt owner to postgres", None),
    (
        "alter table pt attach partition tt for values from (5) to (6)",
        None,
    ),
    (
        "alter table pt detach partition tt",
        Some("relation \"tt\" is not a partition of relation \"pt\""),
    ),
    (
        "alter table pt options (add x 'y')",
        Some(
            "ALTER action OPTIONS cannot be performed on relation \"pt\" (This operation is not supported for partitioned tables.",
        ),
    ),
    (
        "alter table pt alter column a options (add x 'y')",
        Some(
            "ALTER action ALTER COLUMN ... OPTIONS cannot be performed on relation \"pt\" (This operation is not supported for partitioned tables.",
        ),
    ),
    ("alter table pt rename to zz", None),
    ("alter table pt rename column a to z", None),
    ("alter table pt set schema public", None),
    (
        "alter table pt alter column a set data type int",
        Some(
            "cannot alter column \"a\" because it is part of the partition key of relation \"pt\"",
        ),
    ),
    (
        "alter table pt add column c int generated always as (a + 1) stored",
        None,
    ),
    (
        "alter table ti add column c int",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti drop column b",
        Some(
            "ALTER action DROP COLUMN cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a set default 1",
        Some(
            "ALTER action ALTER COLUMN ... SET DEFAULT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a drop default",
        Some(
            "ALTER action ALTER COLUMN ... SET DEFAULT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a set not null",
        Some(
            "ALTER action ALTER COLUMN ... SET NOT NULL cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a drop not null",
        Some(
            "ALTER action ALTER COLUMN ... DROP NOT NULL cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a type bigint",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a set statistics 5",
        Some("cannot alter statistics on non-expression column \"a\" of index \"ti\""),
    ),
    (
        "alter table ti alter column a set storage plain",
        Some(
            "ALTER action ALTER COLUMN ... SET STORAGE cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a set compression pglz",
        Some(
            "ALTER action ALTER COLUMN ... SET COMPRESSION cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a set (n_distinct = 1)",
        Some(
            "ALTER action ALTER COLUMN ... SET cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a reset (n_distinct)",
        Some(
            "ALTER action ALTER COLUMN ... RESET cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a add generated always as identity",
        Some(
            "ALTER action ALTER COLUMN ... ADD IDENTITY cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a set generated always",
        Some(
            "ALTER action ALTER COLUMN ... SET cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a drop identity",
        Some(
            "ALTER action ALTER COLUMN ... DROP IDENTITY cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a drop expression",
        Some(
            "ALTER action ALTER COLUMN ... DROP EXPRESSION cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a set expression as (1)",
        Some(
            "ALTER action ALTER COLUMN ... SET EXPRESSION cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti add constraint k check (a > 0)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti add primary key (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti add unique (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti add foreign key (a) references tt (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti add constraint k check (a > 0) not valid",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti validate constraint k",
        Some(
            "ALTER action VALIDATE CONSTRAINT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti drop constraint k",
        Some(
            "ALTER action DROP CONSTRAINT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter constraint k deferrable",
        Some(
            "ALTER action ALTER CONSTRAINT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti rename constraint k to k2",
        Some("constraint \"k\" for table \"ti\" does not exist"),
    ),
    (
        "alter table ti cluster on ti",
        Some(
            "ALTER action CLUSTER ON cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti set without cluster",
        Some(
            "ALTER action SET WITHOUT CLUSTER cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti set logged",
        Some(
            "ALTER action SET LOGGED cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti set unlogged",
        Some(
            "ALTER action SET UNLOGGED cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti set without oids",
        Some(
            "ALTER action SET WITHOUT OIDS cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti set access method heap",
        Some(
            "ALTER action SET ACCESS METHOD cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    ("alter table ti set tablespace pg_default", None),
    ("alter table ti set (fillfactor = 50)", None),
    ("alter table ti reset (fillfactor)", None),
    (
        "alter table ti replica identity full",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti replica identity using index ti",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti disable trigger all",
        Some(
            "ALTER action DISABLE TRIGGER ALL cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti enable trigger all",
        Some(
            "ALTER action ENABLE TRIGGER ALL cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti enable trigger user",
        Some(
            "ALTER action ENABLE TRIGGER USER cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti enable always trigger tr",
        Some(
            "ALTER action ENABLE ALWAYS TRIGGER cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti disable trigger tr",
        Some(
            "ALTER action DISABLE TRIGGER cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti enable rule r",
        Some(
            "ALTER action ENABLE RULE cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti disable rule r",
        Some(
            "ALTER action DISABLE RULE cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti enable replica rule r",
        Some(
            "ALTER action ENABLE REPLICA RULE cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti enable row level security",
        Some(
            "ALTER action ENABLE ROW SECURITY cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti disable row level security",
        Some(
            "ALTER action DISABLE ROW SECURITY cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti force row level security",
        Some(
            "ALTER action FORCE ROW SECURITY cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti no force row level security",
        Some(
            "ALTER action NO FORCE ROW SECURITY cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti inherit tt",
        Some(
            "ALTER action INHERIT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti no inherit tt",
        Some(
            "ALTER action NO INHERIT cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti of ct",
        Some(
            "ALTER action OF cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti not of",
        Some(
            "ALTER action NOT OF cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    ("alter table ti owner to postgres", None),
    (
        "alter table ti attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti detach partition tt",
        Some(
            "ALTER action DETACH PARTITION cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti options (add x 'y')",
        Some(
            "ALTER action OPTIONS cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti alter column a options (add x 'y')",
        Some(
            "ALTER action ALTER COLUMN ... OPTIONS cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    ("alter table ti rename to zz", None),
    ("alter table ti rename column a to z", None),
    (
        "alter table ti set schema public",
        Some("cannot change schema of index \"ti\""),
    ),
    (
        "alter table ti alter column a set data type int",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table ti add column c int generated always as (a + 1) stored",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table pti add column c int",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti drop column b",
        Some(
            "ALTER action DROP COLUMN cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a set default 1",
        Some(
            "ALTER action ALTER COLUMN ... SET DEFAULT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a drop default",
        Some(
            "ALTER action ALTER COLUMN ... SET DEFAULT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a set not null",
        Some(
            "ALTER action ALTER COLUMN ... SET NOT NULL cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a drop not null",
        Some(
            "ALTER action ALTER COLUMN ... DROP NOT NULL cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a type bigint",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a set statistics 5",
        Some("cannot alter statistics on non-expression column \"a\" of index \"pti\""),
    ),
    (
        "alter table pti alter column a set storage plain",
        Some(
            "ALTER action ALTER COLUMN ... SET STORAGE cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a set compression pglz",
        Some(
            "ALTER action ALTER COLUMN ... SET COMPRESSION cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a set (n_distinct = 1)",
        Some(
            "ALTER action ALTER COLUMN ... SET cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a reset (n_distinct)",
        Some(
            "ALTER action ALTER COLUMN ... RESET cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a add generated always as identity",
        Some(
            "ALTER action ALTER COLUMN ... ADD IDENTITY cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a set generated always",
        Some(
            "ALTER action ALTER COLUMN ... SET cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a drop identity",
        Some(
            "ALTER action ALTER COLUMN ... DROP IDENTITY cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a drop expression",
        Some(
            "ALTER action ALTER COLUMN ... DROP EXPRESSION cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a set expression as (1)",
        Some(
            "ALTER action ALTER COLUMN ... SET EXPRESSION cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti add constraint k check (a > 0)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti add primary key (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti add unique (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti add foreign key (a) references tt (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti add constraint k check (a > 0) not valid",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti validate constraint k",
        Some(
            "ALTER action VALIDATE CONSTRAINT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti drop constraint k",
        Some(
            "ALTER action DROP CONSTRAINT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter constraint k deferrable",
        Some(
            "ALTER action ALTER CONSTRAINT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti rename constraint k to k2",
        Some("constraint \"k\" for table \"pti\" does not exist"),
    ),
    (
        "alter table pti cluster on ti",
        Some(
            "ALTER action CLUSTER ON cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti set without cluster",
        Some(
            "ALTER action SET WITHOUT CLUSTER cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti set logged",
        Some(
            "ALTER action SET LOGGED cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti set unlogged",
        Some(
            "ALTER action SET UNLOGGED cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti set without oids",
        Some(
            "ALTER action SET WITHOUT OIDS cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti set access method heap",
        Some(
            "ALTER action SET ACCESS METHOD cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    ("alter table pti set tablespace pg_default", None),
    (
        "alter table pti set (fillfactor = 50)",
        Some(
            "ALTER action SET cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti reset (fillfactor)",
        Some(
            "ALTER action RESET cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti replica identity full",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti replica identity using index ti",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti disable trigger all",
        Some(
            "ALTER action DISABLE TRIGGER ALL cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti enable trigger all",
        Some(
            "ALTER action ENABLE TRIGGER ALL cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti enable trigger user",
        Some(
            "ALTER action ENABLE TRIGGER USER cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti enable always trigger tr",
        Some(
            "ALTER action ENABLE ALWAYS TRIGGER cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti disable trigger tr",
        Some(
            "ALTER action DISABLE TRIGGER cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti enable rule r",
        Some(
            "ALTER action ENABLE RULE cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti disable rule r",
        Some(
            "ALTER action DISABLE RULE cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti enable replica rule r",
        Some(
            "ALTER action ENABLE REPLICA RULE cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti enable row level security",
        Some(
            "ALTER action ENABLE ROW SECURITY cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti disable row level security",
        Some(
            "ALTER action DISABLE ROW SECURITY cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti force row level security",
        Some(
            "ALTER action FORCE ROW SECURITY cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti no force row level security",
        Some(
            "ALTER action NO FORCE ROW SECURITY cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti inherit tt",
        Some(
            "ALTER action INHERIT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti no inherit tt",
        Some(
            "ALTER action NO INHERIT cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti of ct",
        Some(
            "ALTER action OF cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti not of",
        Some(
            "ALTER action NOT OF cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti owner to postgres",
        Some("cannot change owner of index \"pti\""),
    ),
    (
        "alter table pti attach partition tt for values from (5) to (6)",
        Some("\"pti\" is not a partitioned table"),
    ),
    (
        "alter table pti detach partition tt",
        Some(
            "ALTER action DETACH PARTITION cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti options (add x 'y')",
        Some(
            "ALTER action OPTIONS cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti alter column a options (add x 'y')",
        Some(
            "ALTER action ALTER COLUMN ... OPTIONS cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    ("alter table pti rename to zz", None),
    ("alter table pti rename column a to z", None),
    (
        "alter table pti set schema public",
        Some("cannot change schema of index \"pti\""),
    ),
    (
        "alter table pti alter column a set data type int",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table pti add column c int generated always as (a + 1) stored",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table s add column c int",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s drop column b",
        Some(
            "ALTER action DROP COLUMN cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a set default 1",
        Some(
            "ALTER action ALTER COLUMN ... SET DEFAULT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a drop default",
        Some(
            "ALTER action ALTER COLUMN ... SET DEFAULT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a set not null",
        Some(
            "ALTER action ALTER COLUMN ... SET NOT NULL cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a drop not null",
        Some(
            "ALTER action ALTER COLUMN ... DROP NOT NULL cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a type bigint",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a set statistics 5",
        Some(
            "ALTER action ALTER COLUMN ... SET STATISTICS cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a set storage plain",
        Some(
            "ALTER action ALTER COLUMN ... SET STORAGE cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a set compression pglz",
        Some(
            "ALTER action ALTER COLUMN ... SET COMPRESSION cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a set (n_distinct = 1)",
        Some(
            "ALTER action ALTER COLUMN ... SET cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a reset (n_distinct)",
        Some(
            "ALTER action ALTER COLUMN ... RESET cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a add generated always as identity",
        Some(
            "ALTER action ALTER COLUMN ... ADD IDENTITY cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a set generated always",
        Some(
            "ALTER action ALTER COLUMN ... SET cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a drop identity",
        Some(
            "ALTER action ALTER COLUMN ... DROP IDENTITY cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a drop expression",
        Some(
            "ALTER action ALTER COLUMN ... DROP EXPRESSION cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a set expression as (1)",
        Some(
            "ALTER action ALTER COLUMN ... SET EXPRESSION cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s add constraint k check (a > 0)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s add primary key (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s add unique (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s add foreign key (a) references tt (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s add constraint k check (a > 0) not valid",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s validate constraint k",
        Some(
            "ALTER action VALIDATE CONSTRAINT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s drop constraint k",
        Some(
            "ALTER action DROP CONSTRAINT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter constraint k deferrable",
        Some(
            "ALTER action ALTER CONSTRAINT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s rename constraint k to k2",
        Some(
            "cannot rename columns of relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s cluster on ti",
        Some(
            "ALTER action CLUSTER ON cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s set without cluster",
        Some(
            "ALTER action SET WITHOUT CLUSTER cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    ("alter table s set logged", None),
    ("alter table s set unlogged", None),
    (
        "alter table s set without oids",
        Some(
            "ALTER action SET WITHOUT OIDS cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s set access method heap",
        Some(
            "ALTER action SET ACCESS METHOD cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s set tablespace pg_default",
        Some(
            "ALTER action SET TABLESPACE cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s set (fillfactor = 50)",
        Some(
            "ALTER action SET cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s reset (fillfactor)",
        Some(
            "ALTER action RESET cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s replica identity full",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s replica identity using index ti",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s disable trigger all",
        Some(
            "ALTER action DISABLE TRIGGER ALL cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s enable trigger all",
        Some(
            "ALTER action ENABLE TRIGGER ALL cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s enable trigger user",
        Some(
            "ALTER action ENABLE TRIGGER USER cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s enable always trigger tr",
        Some(
            "ALTER action ENABLE ALWAYS TRIGGER cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s disable trigger tr",
        Some(
            "ALTER action DISABLE TRIGGER cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s enable rule r",
        Some(
            "ALTER action ENABLE RULE cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s disable rule r",
        Some(
            "ALTER action DISABLE RULE cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s enable replica rule r",
        Some(
            "ALTER action ENABLE REPLICA RULE cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s enable row level security",
        Some(
            "ALTER action ENABLE ROW SECURITY cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s disable row level security",
        Some(
            "ALTER action DISABLE ROW SECURITY cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s force row level security",
        Some(
            "ALTER action FORCE ROW SECURITY cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s no force row level security",
        Some(
            "ALTER action NO FORCE ROW SECURITY cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s inherit tt",
        Some(
            "ALTER action INHERIT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s no inherit tt",
        Some(
            "ALTER action NO INHERIT cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s of ct",
        Some(
            "ALTER action OF cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s not of",
        Some(
            "ALTER action NOT OF cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    ("alter table s owner to postgres", None),
    (
        "alter table s attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s detach partition tt",
        Some(
            "ALTER action DETACH PARTITION cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s options (add x 'y')",
        Some(
            "ALTER action OPTIONS cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s alter column a options (add x 'y')",
        Some(
            "ALTER action ALTER COLUMN ... OPTIONS cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    ("alter table s rename to zz", None),
    (
        "alter table s rename column a to z",
        Some(
            "cannot rename columns of relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    ("alter table s set schema public", None),
    (
        "alter table s alter column a set data type int",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table s add column c int generated always as (a + 1) stored",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table v add column c int",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v drop column b",
        Some(
            "ALTER action DROP COLUMN cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    ("alter table v alter column a set default 1", None),
    ("alter table v alter column a drop default", None),
    (
        "alter table v alter column a set not null",
        Some(
            "ALTER action ALTER COLUMN ... SET NOT NULL cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a drop not null",
        Some(
            "ALTER action ALTER COLUMN ... DROP NOT NULL cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a type bigint",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a set statistics 5",
        Some(
            "ALTER action ALTER COLUMN ... SET STATISTICS cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a set storage plain",
        Some(
            "ALTER action ALTER COLUMN ... SET STORAGE cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a set compression pglz",
        Some(
            "ALTER action ALTER COLUMN ... SET COMPRESSION cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a set (n_distinct = 1)",
        Some(
            "ALTER action ALTER COLUMN ... SET cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a reset (n_distinct)",
        Some(
            "ALTER action ALTER COLUMN ... RESET cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a add generated always as identity",
        Some(
            "column \"a\" of relation \"v\" must be declared NOT NULL before identity can be added",
        ),
    ),
    (
        "alter table v alter column a set generated always",
        Some("column \"a\" of relation \"v\" is not an identity column"),
    ),
    (
        "alter table v alter column a drop identity",
        Some("column \"a\" of relation \"v\" is not an identity column"),
    ),
    (
        "alter table v alter column a drop expression",
        Some(
            "ALTER action ALTER COLUMN ... DROP EXPRESSION cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a set expression as (1)",
        Some(
            "ALTER action ALTER COLUMN ... SET EXPRESSION cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v add constraint k check (a > 0)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v add primary key (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v add unique (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v add foreign key (a) references tt (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v add constraint k check (a > 0) not valid",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v validate constraint k",
        Some(
            "ALTER action VALIDATE CONSTRAINT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v drop constraint k",
        Some(
            "ALTER action DROP CONSTRAINT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter constraint k deferrable",
        Some(
            "ALTER action ALTER CONSTRAINT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v rename constraint k to k2",
        Some("constraint \"k\" for table \"v\" does not exist"),
    ),
    (
        "alter table v cluster on ti",
        Some(
            "ALTER action CLUSTER ON cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v set without cluster",
        Some(
            "ALTER action SET WITHOUT CLUSTER cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v set logged",
        Some(
            "ALTER action SET LOGGED cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v set unlogged",
        Some(
            "ALTER action SET UNLOGGED cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v set without oids",
        Some(
            "ALTER action SET WITHOUT OIDS cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v set access method heap",
        Some(
            "ALTER action SET ACCESS METHOD cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v set tablespace pg_default",
        Some(
            "ALTER action SET TABLESPACE cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v set (fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    ("alter table v reset (fillfactor)", None),
    (
        "alter table v replica identity full",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v replica identity using index ti",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v disable trigger all",
        Some(
            "ALTER action DISABLE TRIGGER ALL cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v enable trigger all",
        Some(
            "ALTER action ENABLE TRIGGER ALL cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v enable trigger user",
        Some(
            "ALTER action ENABLE TRIGGER USER cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v enable always trigger tr",
        Some(
            "ALTER action ENABLE ALWAYS TRIGGER cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v disable trigger tr",
        Some(
            "ALTER action DISABLE TRIGGER cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v enable rule r",
        Some(
            "ALTER action ENABLE RULE cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v disable rule r",
        Some(
            "ALTER action DISABLE RULE cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v enable replica rule r",
        Some(
            "ALTER action ENABLE REPLICA RULE cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v enable row level security",
        Some(
            "ALTER action ENABLE ROW SECURITY cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v disable row level security",
        Some(
            "ALTER action DISABLE ROW SECURITY cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v force row level security",
        Some(
            "ALTER action FORCE ROW SECURITY cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v no force row level security",
        Some(
            "ALTER action NO FORCE ROW SECURITY cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v inherit tt",
        Some(
            "ALTER action INHERIT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v no inherit tt",
        Some(
            "ALTER action NO INHERIT cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v of ct",
        Some(
            "ALTER action OF cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v not of",
        Some(
            "ALTER action NOT OF cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    ("alter table v owner to postgres", None),
    (
        "alter table v attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v detach partition tt",
        Some(
            "ALTER action DETACH PARTITION cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v options (add x 'y')",
        Some(
            "ALTER action OPTIONS cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v alter column a options (add x 'y')",
        Some(
            "ALTER action ALTER COLUMN ... OPTIONS cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    ("alter table v rename to zz", None),
    ("alter table v rename column a to z", None),
    ("alter table v set schema public", None),
    (
        "alter table v alter column a set data type int",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table v add column c int generated always as (a + 1) stored",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table mv add column c int",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv drop column b",
        Some(
            "ALTER action DROP COLUMN cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a set default 1",
        Some(
            "ALTER action ALTER COLUMN ... SET DEFAULT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a drop default",
        Some(
            "ALTER action ALTER COLUMN ... SET DEFAULT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a set not null",
        Some(
            "ALTER action ALTER COLUMN ... SET NOT NULL cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a drop not null",
        Some(
            "ALTER action ALTER COLUMN ... DROP NOT NULL cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a type bigint",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    ("alter table mv alter column a set statistics 5", None),
    ("alter table mv alter column a set storage plain", None),
    (
        "alter table mv alter column a set compression pglz",
        Some("column data type integer does not support compression"),
    ),
    ("alter table mv alter column a set (n_distinct = 1)", None),
    ("alter table mv alter column a reset (n_distinct)", None),
    (
        "alter table mv alter column a add generated always as identity",
        Some(
            "ALTER action ALTER COLUMN ... ADD IDENTITY cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a set generated always",
        Some(
            "ALTER action ALTER COLUMN ... SET cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a drop identity",
        Some(
            "ALTER action ALTER COLUMN ... DROP IDENTITY cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a drop expression",
        Some(
            "ALTER action ALTER COLUMN ... DROP EXPRESSION cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a set expression as (1)",
        Some(
            "ALTER action ALTER COLUMN ... SET EXPRESSION cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv add constraint k check (a > 0)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv add primary key (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv add unique (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv add foreign key (a) references tt (a)",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv add constraint k check (a > 0) not valid",
        Some(
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv validate constraint k",
        Some(
            "ALTER action VALIDATE CONSTRAINT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv drop constraint k",
        Some(
            "ALTER action DROP CONSTRAINT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter constraint k deferrable",
        Some(
            "ALTER action ALTER CONSTRAINT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv rename constraint k to k2",
        Some("constraint \"k\" for table \"mv\" does not exist"),
    ),
    (
        "alter table mv cluster on ti",
        Some("\"ti\" is not an index for table \"mv\""),
    ),
    ("alter table mv set without cluster", None),
    (
        "alter table mv set logged",
        Some(
            "ALTER action SET LOGGED cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv set unlogged",
        Some(
            "ALTER action SET UNLOGGED cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv set without oids",
        Some(
            "ALTER action SET WITHOUT OIDS cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    ("alter table mv set access method heap", None),
    ("alter table mv set tablespace pg_default", None),
    ("alter table mv set (fillfactor = 50)", None),
    ("alter table mv reset (fillfactor)", None),
    ("alter table mv replica identity full", None),
    (
        "alter table mv replica identity using index ti",
        Some("\"ti\" is not an index for table \"mv\""),
    ),
    (
        "alter table mv disable trigger all",
        Some(
            "ALTER action DISABLE TRIGGER ALL cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv enable trigger all",
        Some(
            "ALTER action ENABLE TRIGGER ALL cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv enable trigger user",
        Some(
            "ALTER action ENABLE TRIGGER USER cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv enable always trigger tr",
        Some(
            "ALTER action ENABLE ALWAYS TRIGGER cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv disable trigger tr",
        Some(
            "ALTER action DISABLE TRIGGER cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv enable rule r",
        Some(
            "ALTER action ENABLE RULE cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv disable rule r",
        Some(
            "ALTER action DISABLE RULE cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv enable replica rule r",
        Some(
            "ALTER action ENABLE REPLICA RULE cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv enable row level security",
        Some(
            "ALTER action ENABLE ROW SECURITY cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv disable row level security",
        Some(
            "ALTER action DISABLE ROW SECURITY cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv force row level security",
        Some(
            "ALTER action FORCE ROW SECURITY cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv no force row level security",
        Some(
            "ALTER action NO FORCE ROW SECURITY cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv inherit tt",
        Some(
            "ALTER action INHERIT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv no inherit tt",
        Some(
            "ALTER action NO INHERIT cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv of ct",
        Some(
            "ALTER action OF cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv not of",
        Some(
            "ALTER action NOT OF cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    ("alter table mv owner to postgres", None),
    (
        "alter table mv attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv detach partition tt",
        Some(
            "ALTER action DETACH PARTITION cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv options (add x 'y')",
        Some(
            "ALTER action OPTIONS cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv alter column a options (add x 'y')",
        Some(
            "ALTER action ALTER COLUMN ... OPTIONS cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    ("alter table mv rename to zz", None),
    ("alter table mv rename column a to z", None),
    ("alter table mv set schema public", None),
    (
        "alter table mv alter column a set data type int",
        Some(
            "ALTER action ALTER COLUMN ... SET DATA TYPE cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table mv add column c int generated always as (a + 1) stored",
        Some(
            "ALTER action ADD COLUMN cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    ("alter table ft add column c int", None),
    ("alter table ft drop column b", None),
    ("alter table ft alter column a set default 1", None),
    ("alter table ft alter column a drop default", None),
    ("alter table ft alter column a set not null", None),
    ("alter table ft alter column a drop not null", None),
    ("alter table ft alter column a type bigint", None),
    ("alter table ft alter column a set statistics 5", None),
    ("alter table ft alter column a set storage plain", None),
    (
        "alter table ft alter column a set compression pglz",
        Some(
            "ALTER action ALTER COLUMN ... SET COMPRESSION cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    ("alter table ft alter column a set (n_distinct = 1)", None),
    ("alter table ft alter column a reset (n_distinct)", None),
    (
        "alter table ft alter column a add generated always as identity",
        Some(
            "column \"a\" of relation \"ft\" must be declared NOT NULL before identity can be added",
        ),
    ),
    (
        "alter table ft alter column a set generated always",
        Some("column \"a\" of relation \"ft\" is not an identity column"),
    ),
    (
        "alter table ft alter column a drop identity",
        Some("column \"a\" of relation \"ft\" is not an identity column"),
    ),
    (
        "alter table ft alter column a drop expression",
        Some("column \"a\" of relation \"ft\" is not a generated column"),
    ),
    (
        "alter table ft alter column a set expression as (1)",
        Some("column \"a\" of relation \"ft\" is not a generated column"),
    ),
    ("alter table ft add constraint k check (a > 0)", None),
    (
        "alter table ft add primary key (a)",
        Some("primary key constraints are not supported on foreign tables"),
    ),
    (
        "alter table ft add unique (a)",
        Some("unique constraints are not supported on foreign tables"),
    ),
    (
        "alter table ft add foreign key (a) references tt (a)",
        Some("foreign key constraints are not supported on foreign tables"),
    ),
    (
        "alter table ft add constraint k check (a > 0) not valid",
        None,
    ),
    (
        "alter table ft validate constraint k",
        Some("constraint \"k\" of relation \"ft\" does not exist"),
    ),
    (
        "alter table ft drop constraint k",
        Some("constraint \"k\" of relation \"ft\" does not exist"),
    ),
    (
        "alter table ft alter constraint k deferrable",
        Some(
            "ALTER action ALTER CONSTRAINT cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft rename constraint k to k2",
        Some("constraint \"k\" for table \"ft\" does not exist"),
    ),
    (
        "alter table ft cluster on ti",
        Some(
            "ALTER action CLUSTER ON cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft set without cluster",
        Some(
            "ALTER action SET WITHOUT CLUSTER cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft set logged",
        Some(
            "ALTER action SET LOGGED cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft set unlogged",
        Some(
            "ALTER action SET UNLOGGED cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    ("alter table ft set without oids", None),
    (
        "alter table ft set access method heap",
        Some(
            "ALTER action SET ACCESS METHOD cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft set tablespace pg_default",
        Some(
            "ALTER action SET TABLESPACE cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft set (fillfactor = 50)",
        Some(
            "ALTER action SET cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft reset (fillfactor)",
        Some(
            "ALTER action RESET cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft replica identity full",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft replica identity using index ti",
        Some(
            "ALTER action REPLICA IDENTITY cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    ("alter table ft disable trigger all", None),
    ("alter table ft enable trigger all", None),
    ("alter table ft enable trigger user", None),
    (
        "alter table ft enable always trigger tr",
        Some("trigger \"tr\" for table \"ft\" does not exist"),
    ),
    (
        "alter table ft disable trigger tr",
        Some("trigger \"tr\" for table \"ft\" does not exist"),
    ),
    (
        "alter table ft enable rule r",
        Some(
            "ALTER action ENABLE RULE cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft disable rule r",
        Some(
            "ALTER action DISABLE RULE cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft enable replica rule r",
        Some(
            "ALTER action ENABLE REPLICA RULE cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft enable row level security",
        Some(
            "ALTER action ENABLE ROW SECURITY cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft disable row level security",
        Some(
            "ALTER action DISABLE ROW SECURITY cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft force row level security",
        Some(
            "ALTER action FORCE ROW SECURITY cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft no force row level security",
        Some(
            "ALTER action NO FORCE ROW SECURITY cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    ("alter table ft inherit tt", None),
    (
        "alter table ft no inherit tt",
        Some("relation \"tt\" is not a parent of relation \"ft\""),
    ),
    (
        "alter table ft of ct",
        Some(
            "ALTER action OF cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft not of",
        Some(
            "ALTER action NOT OF cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    ("alter table ft owner to postgres", None),
    (
        "alter table ft attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table ft detach partition tt",
        Some(
            "ALTER action DETACH PARTITION cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    ("alter table ft options (add x 'y')", None),
    ("alter table ft alter column a options (add x 'y')", None),
    ("alter table ft rename to zz", None),
    ("alter table ft rename column a to z", None),
    ("alter table ft set schema public", None),
    ("alter table ft alter column a set data type int", None),
    (
        "alter table ft add column c int generated always as (a + 1) stored",
        None,
    ),
    (
        "alter table ct add column c int",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct drop column b",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a set default 1",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a drop default",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a set not null",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a drop not null",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a type bigint",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a set statistics 5",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a set storage plain",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a set compression pglz",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a set (n_distinct = 1)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a reset (n_distinct)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a add generated always as identity",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a set generated always",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a drop identity",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a drop expression",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a set expression as (1)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct add constraint k check (a > 0)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct add primary key (a)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct add unique (a)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct add foreign key (a) references tt (a)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct add constraint k check (a > 0) not valid",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct validate constraint k",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct drop constraint k",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter constraint k deferrable",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct rename constraint k to k2",
        Some("constraint \"k\" for table \"ct\" does not exist"),
    ),
    (
        "alter table ct cluster on ti",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct set without cluster",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct set logged",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct set unlogged",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct set without oids",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct set access method heap",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct set tablespace pg_default",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct set (fillfactor = 50)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct reset (fillfactor)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct replica identity full",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct replica identity using index ti",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct disable trigger all",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct enable trigger all",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct enable trigger user",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct enable always trigger tr",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct disable trigger tr",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct enable rule r",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct disable rule r",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct enable replica rule r",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct enable row level security",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct disable row level security",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct force row level security",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct no force row level security",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct inherit tt",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct no inherit tt",
        Some("\"ct\" is a composite type"),
    ),
    ("alter table ct of ct", Some("\"ct\" is a composite type")),
    ("alter table ct not of", Some("\"ct\" is a composite type")),
    (
        "alter table ct owner to postgres",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct attach partition tt for values from (5) to (6)",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct detach partition tt",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct options (add x 'y')",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a options (add x 'y')",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct rename to zz",
        Some("\"ct\" is a composite type"),
    ),
    ("alter table ct rename column a to z", None),
    (
        "alter table ct set schema public",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct alter column a set data type int",
        Some("\"ct\" is a composite type"),
    ),
    (
        "alter table ct add column c int generated always as (a + 1) stored",
        Some("\"ct\" is a composite type"),
    ),
    (
        "reindex table concurrently pg_class",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex index concurrently pg_class_oid_index",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex table concurrently pg_toast.pg_toast_2620",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex index concurrently pg_toast.pg_toast_2620_index",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex table concurrently pg_database",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex index concurrently pg_database_oid_index",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex (concurrently) table pg_class",
        Some("cannot reindex system catalogs concurrently"),
    ),
    ("reindex table concurrently t", None),
    ("reindex index concurrently ti", None),
    ("reindex schema concurrently pg_catalog", None),
    (
        "reindex system concurrently",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex table concurrently information_schema.sql_features",
        None,
    ),
    (
        "reindex index concurrently information_schema.sql_features_pkey",
        Some("relation \"information_schema.sql_features_pkey\" does not exist"),
    ),
    (
        "create rule r as on insert to ft do nothing",
        Some(
            "relation \"ft\" cannot have rules (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "create rule r as on insert to ft do instead nothing",
        Some(
            "relation \"ft\" cannot have rules (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "create rule \"_RETURN\" as on select to ft do instead select 1 as a, 2 as b",
        Some(
            "relation \"ft\" cannot have rules (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "create rule r as on insert to s do nothing",
        Some("relation \"s\" cannot have rules (This operation is not supported for sequences."),
    ),
    (
        "create rule r as on insert to mv do nothing",
        Some("rules on materialized views are not supported"),
    ),
    (
        "create rule r as on insert to ct do nothing",
        Some("cannot open relation \"ct\" (This operation is not supported for composite types."),
    ),
    ("create rule r as on insert to pt do nothing", None),
    (
        "alter table pti attach partition tt for values from (5) to (6)",
        Some("\"pti\" is not a partitioned table"),
    ),
    (
        "alter table ti attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter table s attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"s\" (This operation is not supported for sequences.",
        ),
    ),
    (
        "alter table v attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"v\" (This operation is not supported for views.",
        ),
    ),
    (
        "alter table mv attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"mv\" (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table ft attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"ft\" (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter table t attach partition tt for values from (5) to (6)",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"t\" (This operation is not supported for tables.",
        ),
    ),
    (
        "alter table pti detach partition tt",
        Some(
            "ALTER action DETACH PARTITION cannot be performed on relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "alter table ti detach partition tt",
        Some(
            "ALTER action DETACH PARTITION cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "alter index pti attach partition ti",
        Some(
            "cannot attach index \"ti\" as a partition of index \"pti\" (Index \"ti\" is not an index on any partition of table \"pt\".",
        ),
    ),
    (
        "alter index ti attach partition pti",
        Some(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"ti\" (This operation is not supported for indexes.",
        ),
    ),
    (
        "create table r partition of s for values in (1)",
        Some("inherited relation \"s\" is not a table or foreign table"),
    ),
    (
        "create table r partition of v for values in (1)",
        Some("inherited relation \"v\" is not a table or foreign table"),
    ),
    (
        "create table r partition of mv for values in (1)",
        Some("inherited relation \"mv\" is not a table or foreign table"),
    ),
    (
        "create table r partition of ft for values in (1)",
        Some("\"ft\" is not partitioned"),
    ),
    (
        "create table r partition of t for values in (1)",
        Some("\"t\" is not partitioned"),
    ),
    (
        "create table r partition of ct for values in (1)",
        Some("cannot open relation \"ct\" (This operation is not supported for composite types."),
    ),
    (
        "create table r () inherits (s)",
        Some("inherited relation \"s\" is not a table or foreign table"),
    ),
    (
        "create table r () inherits (v)",
        Some("inherited relation \"v\" is not a table or foreign table"),
    ),
    (
        "create table r () inherits (mv)",
        Some("inherited relation \"mv\" is not a table or foreign table"),
    ),
    (
        "create foreign table r partition of s for values in (1) server sv",
        Some("inherited relation \"s\" is not a table or foreign table"),
    ),
    (
        "alter table tt add constraint k primary key using index t",
        Some("\"t\" is not an index"),
    ),
    (
        "alter table tt add constraint k unique using index s",
        Some("\"s\" is not an index"),
    ),
    (
        "alter table tt add constraint k unique using index v",
        Some("\"v\" is not an index"),
    ),
    (
        "alter table tt add constraint k unique using index nosuch",
        Some("index \"nosuch\" does not exist"),
    ),
    (
        "alter table tt add constraint k unique using index ti",
        Some("index \"ti\" does not belong to table \"tt\""),
    ),
    (
        "alter table tt add primary key using index mv",
        Some("\"mv\" is not an index"),
    ),
    (
        "alter table tt add constraint k unique using index pti",
        Some("index \"pti\" does not belong to table \"tt\""),
    ),
    (
        "alter table tt add constraint k unique using index ct",
        Some("\"ct\" is not an index"),
    ),
    (
        "alter trigger tr on s rename to tr2",
        Some("relation \"s\" cannot have triggers (This operation is not supported for sequences."),
    ),
    (
        "alter trigger tr on mv rename to tr2",
        Some(
            "relation \"mv\" cannot have triggers (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter trigger tr on ct rename to tr2",
        Some(
            "relation \"ct\" cannot have triggers (This operation is not supported for composite types.",
        ),
    ),
    (
        "alter trigger tr on ti rename to tr2",
        Some("relation \"ti\" cannot have triggers (This operation is not supported for indexes."),
    ),
    (
        "alter trigger tr on v rename to tr2",
        Some("trigger \"tr\" for table \"v\" does not exist"),
    ),
    (
        "alter trigger tr on ft rename to tr2",
        Some("trigger \"tr\" for table \"ft\" does not exist"),
    ),
    (
        "alter rule r on s rename to r2",
        Some("relation \"s\" cannot have rules (This operation is not supported for sequences."),
    ),
    (
        "alter rule r on mv rename to r2",
        Some(
            "relation \"mv\" cannot have rules (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter rule r on ti rename to r2",
        Some("relation \"ti\" cannot have rules (This operation is not supported for indexes."),
    ),
    (
        "alter rule r on ct rename to r2",
        Some(
            "relation \"ct\" cannot have rules (This operation is not supported for composite types.",
        ),
    ),
    (
        "alter rule r on ft rename to r2",
        Some(
            "relation \"ft\" cannot have rules (This operation is not supported for foreign tables.",
        ),
    ),
    (
        "alter rule r on v rename to r2",
        Some("rule \"r\" for relation \"v\" does not exist"),
    ),
    (
        "alter rule r on pti rename to r2",
        Some(
            "relation \"pti\" cannot have rules (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "create foreign table f2 (a int primary key) server sv",
        Some("primary key constraints are not supported on foreign tables"),
    ),
    (
        "create foreign table f2 (a int unique) server sv",
        Some("unique constraints are not supported on foreign tables"),
    ),
    (
        "create foreign table f2 (a int references tt (a)) server sv",
        Some("foreign key constraints are not supported on foreign tables"),
    ),
    (
        "create foreign table f2 (a int, primary key (a)) server sv",
        Some("primary key constraints are not supported on foreign tables"),
    ),
    (
        "create foreign table f2 (a int, exclude (a with =)) server sv",
        Some("exclusion constraints are not supported on foreign tables"),
    ),
    (
        "create foreign table f2 (a int check (a > 0)) server sv",
        None,
    ),
    ("create foreign table f2 (a int not null) server sv", None),
    (
        "alter table ft add constraint x exclude (a with =)",
        Some("exclusion constraints are not supported on foreign tables"),
    ),
    (
        "alter table ft add constraint x check (a > 0) not valid",
        None,
    ),
    (
        "alter table ft add unique (a) deferrable",
        Some("unique constraints are not supported on foreign tables"),
    ),
    (
        "alter table pt cluster on pti",
        Some("cannot mark index clustered in partitioned table"),
    ),
    ("alter table pt of ct", None),
    ("alter table pt not of", Some("\"pt\" is not a typed table")),
    (
        "alter table v alter column a add generated always as identity",
        Some(
            "column \"a\" of relation \"v\" must be declared NOT NULL before identity can be added",
        ),
    ),
    (
        "create trigger tg before insert on s for each row execute function trf()",
        Some("relation \"s\" cannot have triggers (This operation is not supported for sequences."),
    ),
    (
        "create trigger tg before insert on mv for each row execute function trf()",
        Some(
            "relation \"mv\" cannot have triggers (This operation is not supported for materialized views.",
        ),
    ),
    (
        "alter table t replica identity using index nosuch",
        Some("index \"nosuch\" for table \"t\" does not exist"),
    ),
    ("alter table mv replica identity full", None),
    (
        "alter table mv cluster on ti",
        Some("\"ti\" is not an index for table \"mv\""),
    ),
    ("alter table ti set (fillfactor = 50)", None),
    ("alter view v set (security_barrier)", None),
    ("alter index ti set (fillfactor = 50)", None),
    ("alter materialized view mv set (fillfactor = 50)", None),
    (
        "reindex (concurrently true) index pg_class_oid_index",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex (concurrently false) index pg_class_oid_index",
        None,
    ),
    (
        "reindex index concurrently pg_class_relname_nsp_index",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex table concurrently pg_catalog.pg_trigger",
        Some("cannot reindex system catalogs concurrently"),
    ),
    (
        "reindex index concurrently nosuch",
        Some("relation \"nosuch\" does not exist"),
    ),
    (
        "reindex index concurrently t",
        Some("\"t\" is not an index"),
    ),
    (
        "alter table ft add column x int primary key",
        Some("primary key constraints are not supported on foreign tables"),
    ),
    (
        "alter foreign table ft add column x int references tt (a)",
        Some("foreign key constraints are not supported on foreign tables"),
    ),
    (
        "alter foreign table ft add constraint k unique (a)",
        Some("unique constraints are not supported on foreign tables"),
    ),
    ("alter table ft add column x int check (x > 0)", None),
    (
        "alter table pti owner to postgres",
        Some("cannot change owner of index \"pti\""),
    ),
    (
        "alter index pti owner to postgres",
        Some("cannot change owner of index \"pti\""),
    ),
    ("alter index ti owner to postgres", None),
    (
        "alter table pt set without cluster",
        Some("cannot mark index clustered in partitioned table"),
    ),
    (
        "alter table pt cluster on pti",
        Some("cannot mark index clustered in partitioned table"),
    ),
    (
        "create table r partition of pti for values in (1)",
        Some(
            "cannot open relation \"pti\" (This operation is not supported for partitioned indexes.",
        ),
    ),
    (
        "create table r partition of ti for values in (1)",
        Some("cannot open relation \"ti\" (This operation is not supported for indexes."),
    ),
    (
        "alter table tt add constraint k unique using index pt",
        Some("\"pt\" is not an index"),
    ),
    (
        "alter table tt add constraint k unique using index ft",
        Some("\"ft\" is not an index"),
    ),
    ("reindex (concurrently off) table pg_class", None),
];

#[test]
fn a_foreign_table_refuses_index_constraints_only_where_it_is_named() {
    // transformColumnDefinition / transformTableConstraint refuse a primary
    // key, unique, exclusion or foreign key constraint on the foreign table
    // a statement names — not on a foreign inheritance child ADD COLUMN
    // recurses to (such constraints aren't inherited), and not once IF NOT
    // EXISTS skipped the statement. An inline foreign key reaches a
    // partitioned table's partitions, as ADD CONSTRAINT's does.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE FOREIGN DATA WRAPPER w; CREATE SERVER srv FOREIGN DATA WRAPPER w;
         CREATE TABLE par (a int NOT NULL, b int);
         CREATE FOREIGN TABLE fch (x int) INHERITS (par) SERVER srv;
         CREATE TABLE gpar (a int NOT NULL, b int);
         CREATE TABLE gmid () INHERITS (gpar);
         CREATE FOREIGN TABLE gch () INHERITS (gmid) SERVER srv;
         CREATE FOREIGN TABLE ft (a int, b int) SERVER srv;
         CREATE TABLE ref (a int PRIMARY KEY);
         CREATE TABLE pp (a int NOT NULL, b int) PARTITION BY LIST (a);
         CREATE FOREIGN TABLE ppf PARTITION OF pp FOR VALUES IN (1) SERVER srv;
         CREATE TABLE q (a int NOT NULL, b int) PARTITION BY LIST (a);
         CREATE TABLE q1 PARTITION OF q FOR VALUES IN (1) PARTITION BY LIST (a);
         CREATE TABLE q11 PARTITION OF q1 FOR VALUES IN (1);",
    )]);
    for sql in [
        "ALTER TABLE par ADD COLUMN z int PRIMARY KEY",
        "ALTER TABLE par ADD COLUMN z int UNIQUE",
        "ALTER TABLE par ADD COLUMN z int UNIQUE NOT NULL",
        "ALTER TABLE par ADD COLUMN z int REFERENCES ref (a)",
        "ALTER TABLE par ADD COLUMN IF NOT EXISTS z int PRIMARY KEY",
        "ALTER TABLE gpar ADD COLUMN z int PRIMARY KEY",
        "ALTER TABLE gpar ADD COLUMN z int REFERENCES ref (a)",
        "ALTER TABLE q ADD COLUMN z int REFERENCES ref (a)",
        "ALTER TABLE q ADD COLUMN z int, ADD FOREIGN KEY (z) REFERENCES ref (a)",
    ] {
        if let Err(err) = db.apply_sql(&format!("BEGIN; {sql}; ROLLBACK;")) {
            panic!("{sql}\n  rejected: {err}");
        }
    }
    // Skipped by IF NOT EXISTS: nothing changes.
    db.apply_sql(
        "ALTER TABLE par ADD COLUMN IF NOT EXISTS a int PRIMARY KEY;
         ALTER FOREIGN TABLE ft ADD COLUMN IF NOT EXISTS a int UNIQUE;
         ALTER FOREIGN TABLE ft ADD COLUMN IF NOT EXISTS a int PRIMARY KEY;
         ALTER FOREIGN TABLE ft ADD COLUMN IF NOT EXISTS a int REFERENCES ref (a);
         CREATE FOREIGN TABLE IF NOT EXISTS ft (a int PRIMARY KEY) SERVER srv;
         CREATE FOREIGN TABLE IF NOT EXISTS ft (a int UNIQUE) SERVER srv;
         CREATE FOREIGN TABLE IF NOT EXISTS ft (a int REFERENCES ref (a)) SERVER srv;
         CREATE FOREIGN TABLE IF NOT EXISTS ft (a int, PRIMARY KEY (a)) SERVER srv;
         CREATE FOREIGN TABLE IF NOT EXISTS ft (a int, UNIQUE (a)) SERVER srv;
         CREATE FOREIGN TABLE IF NOT EXISTS ft (a int, EXCLUDE (a WITH =)) SERVER srv;
         CREATE FOREIGN TABLE IF NOT EXISTS ft (a int, FOREIGN KEY (a) REFERENCES ref (a))
             SERVER srv;",
    )
    .unwrap();
    for (sql, message) in [
        (
            "ALTER TABLE ft ADD COLUMN z int PRIMARY KEY;",
            "primary key constraints are not supported on foreign tables",
        ),
        (
            "ALTER FOREIGN TABLE ft ADD COLUMN IF NOT EXISTS z int UNIQUE;",
            "unique constraints are not supported on foreign tables",
        ),
        (
            "ALTER TABLE ft ADD COLUMN z int REFERENCES ref (a);",
            "foreign key constraints are not supported on foreign tables",
        ),
        (
            "CREATE FOREIGN TABLE IF NOT EXISTS f9 (a int PRIMARY KEY) SERVER srv;",
            "primary key constraints are not supported on foreign tables",
        ),
        (
            "CREATE FOREIGN TABLE ft (a int PRIMARY KEY) SERVER srv;",
            "primary key constraints are not supported on foreign tables",
        ),
        (
            "ALTER TABLE pp ADD COLUMN z int PRIMARY KEY;",
            "unique constraint on partitioned table must include all partitioning columns",
        ),
        (
            "ALTER TABLE ONLY q ADD COLUMN z int REFERENCES ref (a);",
            "column must be added to child tables too",
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
fn reindex_options_are_read_as_exec_reindex_reads_them() {
    // defGetBoolean takes no value, true / false / on / off (in any case)
    // or the integers 1 / 0, and nothing else; an unknown option is
    // refused — before anything else is looked at.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int); CREATE INDEX ti ON t (a);",
    )]);
    for sql in [
        "REINDEX (CONCURRENTLY 'on') INDEX ti;",
        "REINDEX (CONCURRENTLY TRUE) INDEX ti;",
        "REINDEX (CONCURRENTLY 0) INDEX ti;",
        "REINDEX (TABLESPACE pg_default) INDEX ti;",
        "SELECT 1; REINDEX (CONCURRENTLY off) INDEX ti;",
        "SELECT 1; REINDEX (VERBOSE false) INDEX ti;",
    ] {
        if let Err(err) = db.apply_sql(sql) {
            panic!("{sql}\n  rejected: {err}");
        }
    }
    for (sql, message) in [
        (
            "REINDEX (CONCURRENTLY maybe) INDEX ti;",
            "concurrently requires a Boolean value",
        ),
        (
            "REINDEX (CONCURRENTLY yes) INDEX ti;",
            "concurrently requires a Boolean value",
        ),
        (
            "REINDEX (CONCURRENTLY 2) INDEX ti;",
            "concurrently requires a Boolean value",
        ),
        (
            "REINDEX (CONCURRENTLY 't') INDEX ti;",
            "concurrently requires a Boolean value",
        ),
        (
            "REINDEX (VERBOSE maybe) INDEX ti;",
            "verbose requires a Boolean value",
        ),
        (
            "REINDEX (foo) INDEX ti;",
            "unrecognized REINDEX option \"foo\"",
        ),
        (
            "REINDEX (CONCURRENTLY maybe) INDEX nosuch;",
            "concurrently requires a Boolean value",
        ),
        (
            "SELECT 1; REINDEX (CONCURRENTLY maybe) INDEX ti;",
            "concurrently requires a Boolean value",
        ),
        (
            "SELECT 1; REINDEX (CONCURRENTLY) INDEX ti;",
            "REINDEX CONCURRENTLY cannot run inside a transaction block",
        ),
    ] {
        let err = db.apply_sql(sql).expect_err(sql);
        assert!(
            err.to_string().starts_with(message),
            "{sql}\n  expected: {message}\n       got: {err}"
        );
    }
}

/// The relations [`FOREIGN_PARTITION_CASES`] run over.
const FOREIGN_PARTITION_SETUP: &str = "
    CREATE FOREIGN DATA WRAPPER w; CREATE SERVER srv FOREIGN DATA WRAPPER w;
    CREATE TABLE pp (a int NOT NULL, b int) PARTITION BY LIST (a);
    CREATE FOREIGN TABLE ppf PARTITION OF pp FOR VALUES IN (1) SERVER srv;
    CREATE TABLE pk (a int PRIMARY KEY, b int) PARTITION BY LIST (a);
    CREATE TABLE ref (a int PRIMARY KEY);
    CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a, NULL::int AS b, 2 AS c;
    CREATE UNIQUE INDEX mv_b ON mv (b);
    CREATE UNIQUE INDEX mv_a ON mv (a);
    CREATE TABLE t (a int, b int);
    CREATE INDEX ti ON t (a);
    CREATE VIEW v AS SELECT 1 AS a;
    CREATE TABLE par (a int, b int);
    CREATE TABLE chi () INHERITS (par);
    CREATE TEMP TABLE tmp (a int) PARTITION BY LIST (a);
    CREATE TABLE lp (a int) PARTITION BY LIST (a);
    CREATE TABLE lp1 PARTITION OF lp FOR VALUES IN (1);";

#[test]
fn foreign_partitions_toast_options_and_relation_checks_follow_pg() {
    // A foreign partition takes none of its parent's indexes (so a unique
    // one can't be created, attached or inherited) nor foreign keys;
    // `toast.` options are taken unchecked by a relation without a TOAST
    // table, checked as a TOAST table's on CREATE TABLE ... PARTITION BY;
    // a materialized view's columns are nullable for REPLICA IDENTITY;
    // ONLY ... ADD COLUMN IF NOT EXISTS skips an existing column before
    // the children are looked at; `pg_temp` is no schema for REINDEX
    // SCHEMA; NOTHING is no SELECT for a rule on SELECT. What PG runs is
    // tried in a transaction rolled back; what it refuses changes nothing.
    let mut db = build_db(&[("0001.sql", FOREIGN_PARTITION_SETUP)]);
    for &(sql, expected) in FOREIGN_PARTITION_CASES {
        match expected {
            None => {
                if let Err(err) = db.apply_sql(&format!("BEGIN; {sql}; ROLLBACK;")) {
                    panic!("{sql}\n  rejected: {err}");
                }
            }
            Some(message) => {
                let err = db.apply_sql(&format!("{sql};")).expect_err(sql);
                assert!(
                    err.to_string().starts_with(message),
                    "{sql}\n  expected: {message}\n       got: {err}"
                );
            }
        }
    }
}

/// `(statement, PG's error)` — `None` when PG runs it — over
/// [`FOREIGN_PARTITION_SETUP`], each checked on PostgreSQL 18.6.
const FOREIGN_PARTITION_CASES: &[(&str, Option<&str>)] = &[
    (
        "alter table pp add primary key (a)",
        Some(
            "cannot create unique index on partitioned table \"pp\" (Table \"pp\" contains partitions that are foreign tables.",
        ),
    ),
    (
        "alter table pp add unique (a)",
        Some(
            "cannot create unique index on partitioned table \"pp\" (Table \"pp\" contains partitions that are foreign tables.",
        ),
    ),
    (
        "alter table pp add constraint u unique (a, b)",
        Some(
            "cannot create unique index on partitioned table \"pp\" (Table \"pp\" contains partitions that are foreign tables.",
        ),
    ),
    (
        "alter table pp add foreign key (b) references ref (a)",
        Some("foreign key constraints are not supported on foreign tables"),
    ),
    ("alter table pp add constraint x exclude (a with =)", None),
    ("alter table only pp add primary key (a)", None),
    (
        "create unique index on pp (a)",
        Some(
            "cannot create unique index on partitioned table \"pp\" (Table \"pp\" contains partitions that are foreign tables.",
        ),
    ),
    ("create index on pp (a)", None),
    ("create unique index on only pp (a)", None),
    (
        "create foreign table pkf partition of pk for values in (1) server srv",
        Some(
            "cannot create foreign partition of partitioned table \"pk\" (Table \"pk\" contains indexes that are unique.",
        ),
    ),
    (
        "create foreign table ppf2 partition of pp for values in (2) server srv",
        None,
    ),
    (
        "create table pf (a int not null, b int references ref (a)) partition by list (a); create foreign table pff partition of pf for values in (1) server srv",
        Some("foreign key constraints are not supported on foreign tables"),
    ),
    (
        "create table pf (a int not null, b int references ref (a)) partition by list (a); create foreign table pff (a int not null, b int) server srv; alter table pf attach partition pff for values in (1)",
        Some("foreign key constraints are not supported on foreign tables"),
    ),
    (
        "create foreign table pkf (a int not null, b int) server srv; alter table pk attach partition pkf for values in (1)",
        Some(
            "cannot attach foreign table \"pkf\" as partition of partitioned table \"pk\" (Partitioned table \"pk\" contains unique indexes.",
        ),
    ),
    (
        "create table pu (a int not null, b int) partition by list (a); create unique index on pu (a); create foreign table puf (a int not null, b int) server srv; alter table pu attach partition puf for values in (1)",
        Some(
            "cannot attach foreign table \"puf\" as partition of partitioned table \"pu\" (Partitioned table \"pu\" contains unique indexes.",
        ),
    ),
    (
        "create table pu (a int not null, b int) partition by list (a); create index on pu (a); create foreign table puf partition of pu for values in (1) server srv",
        None,
    ),
    (
        "create table pu (a int not null, b int) partition by list (a); create index pui on pu (a); create foreign table puf partition of pu for values in (1) server srv; alter index puf_a_idx rename to x",
        Some("relation \"puf_a_idx\" does not exist"),
    ),
    (
        "create table pu (a int not null, b int) partition by list (a); create index pui on pu (a); create foreign table puf (a int not null, b int) server srv; alter table pu attach partition puf for values in (1); alter index puf_a_idx rename to x",
        Some("relation \"puf_a_idx\" does not exist"),
    ),
    (
        "create table pu (a int not null, b int) partition by list (a); create foreign table puf partition of pu for values in (1) server srv; create index pui on pu (a); alter index puf_a_idx rename to x",
        Some("relation \"puf_a_idx\" does not exist"),
    ),
    (
        "create table pu (a int not null, b int) partition by list (a); create foreign table puf partition of pu for values in (1) server srv; create index pui on pu (a); alter table pu detach partition puf",
        None,
    ),
    (
        "create table pn (a int not null, b int) partition by list (a); create table pn1 partition of pn for values in (1) partition by list (a); create foreign table pn11 partition of pn1 for values in (1) server srv; alter table pn add primary key (a)",
        Some(
            "cannot create unique index on partitioned table \"pn1\" (Table \"pn1\" contains partitions that are foreign tables.",
        ),
    ),
    (
        "create table pn (a int not null, b int) partition by list (a); create table pn1 partition of pn for values in (1) partition by list (a); create foreign table pn11 partition of pn1 for values in (1) server srv; alter table pn add unique (a)",
        Some(
            "cannot create unique index on partitioned table \"pn1\" (Table \"pn1\" contains partitions that are foreign tables.",
        ),
    ),
    (
        "create table pn (a int not null, b int) partition by list (a); create table pn1 partition of pn for values in (1) partition by list (a); create foreign table pn11 partition of pn1 for values in (1) server srv; create unique index on pn (a)",
        Some(
            "cannot create unique index on partitioned table \"pn\" (Table \"pn\" contains partitions that are foreign tables.",
        ),
    ),
    (
        "create table pn (a int not null, b int) partition by list (a); create table pn1 partition of pn for values in (1) partition by list (a); create foreign table pn11 partition of pn1 for values in (1) server srv; create unique index on only pn (a)",
        None,
    ),
    (
        "create table pn (a int not null, b int) partition by list (a); create table pn1 partition of pn for values in (1) partition by list (a); create foreign table pn11 partition of pn1 for values in (1) server srv; alter table pn add foreign key (b) references ref (a)",
        Some("foreign key constraints are not supported on foreign tables"),
    ),
    (
        "alter table pp add foreign key (b) references ref (a) not valid",
        Some("foreign key constraints are not supported on foreign tables"),
    ),
    (
        "create table r9 (a int not null, b int) partition by list (a); alter table r9 add foreign key (b) references ref (a); create foreign table r9f partition of r9 for values in (1) server srv",
        Some("foreign key constraints are not supported on foreign tables"),
    ),
    (
        "alter table mv replica identity using index mv_b",
        Some("index \"mv_b\" cannot be used as replica identity because column \"b\" is nullable"),
    ),
    (
        "alter table mv replica identity using index mv_a",
        Some("index \"mv_a\" cannot be used as replica identity because column \"a\" is nullable"),
    ),
    (
        "alter table t replica identity using index ti",
        Some("cannot use non-unique index \"ti\" as replica identity"),
    ),
    ("alter view v set (toast.autovacuum_enabled = false)", None),
    ("alter table v set (toast.autovacuum_enabled = false)", None),
    (
        "alter index ti set (toast.autovacuum_enabled = false)",
        None,
    ),
    (
        "alter table pp set (toast.autovacuum_enabled = false)",
        None,
    ),
    ("alter table t set (toast.autovacuum_enabled = false)", None),
    (
        "alter materialized view mv set (toast.autovacuum_enabled = false)",
        None,
    ),
    (
        "create view v2 with (toast.autovacuum_enabled = false) as select 1",
        None,
    ),
    (
        "create index ti2 on t (a) with (toast.autovacuum_enabled = false)",
        Some("unrecognized parameter namespace \"toast\""),
    ),
    (
        "create table pq (a int) partition by list (a) with (toast.autovacuum_enabled = false)",
        None,
    ),
    ("alter view v reset (toast.autovacuum_enabled)", None),
    ("alter index ti reset (toast.fillfactor)", None),
    ("alter table v set (toast.foo = 1)", None),
    ("alter index ti set (toast.foo = 1)", None),
    ("alter table pp set (toast.foo = 1)", None),
    ("alter table t set (toast.foo = 1)", None),
    ("create view v3 with (toast.foo = 1) as select 1", None),
    (
        "create table pq (a int) partition by list (a) with (toast.foo = 1)",
        Some("unrecognized parameter \"foo\""),
    ),
    (
        "create table pq (a int) partition by list (a) with (toast.autovacuum_enabled = maybe)",
        Some("invalid value for boolean option \"autovacuum_enabled\": maybe"),
    ),
    (
        "create table pq (a int) with (toast.foo = 1)",
        Some("unrecognized parameter \"foo\""),
    ),
    (
        "create materialized view mv2 with (toast.foo = 1) as select 1",
        Some("unrecognized parameter \"foo\""),
    ),
    (
        "alter view v set (toast.foo = 1, check_option = local)",
        Some("WITH CHECK OPTION is supported only on automatically updatable views"),
    ),
    (
        "alter table pp set (toast.autovacuum_enabled = maybe)",
        None,
    ),
    (
        "alter index ti set (toast.autovacuum_enabled = maybe, fillfactor = 50)",
        None,
    ),
    (
        "alter table pp set (toast.x = 1, fillfactor = 50)",
        Some("cannot specify storage parameters for a partitioned table"),
    ),
    (
        "alter view v set (foo.x = 1)",
        Some("unrecognized parameter namespace \"foo\""),
    ),
    ("alter table only par add column if not exists a int", None),
    (
        "alter table only par add column if not exists z int",
        Some("column must be added to child tables too"),
    ),
    (
        "alter table only par add column a int",
        Some("column \"a\" of relation \"par\" already exists"),
    ),
    (
        "alter table only par add column if not exists a int primary key",
        None,
    ),
    (
        "reindex schema pg_temp",
        Some("schema \"pg_temp\" does not exist"),
    ),
    (
        "reindex schema \"pg_temp\"",
        Some("schema \"pg_temp\" does not exist"),
    ),
    (
        "create table tp partition of tmp for values in (1)",
        Some("cannot create a permanent relation as partition of temporary relation \"tmp\""),
    ),
    (
        "create temp table tp partition of tmp for values in (1)",
        None,
    ),
    (
        "create rule \"_RETURN\" as on select to v do instead nothing",
        Some("rules on SELECT must have action INSTEAD SELECT"),
    ),
    (
        "create rule r as on select to v do instead nothing",
        Some("rules on SELECT must have action INSTEAD SELECT"),
    ),
    (
        "create rule r as on select to v do also nothing",
        Some("rules on SELECT must have action INSTEAD SELECT"),
    ),
    (
        "create rule \"_RETURN\" as on select to t do instead nothing",
        Some(
            "relation \"t\" cannot have ON SELECT rules (This operation is not supported for tables.",
        ),
    ),
    (
        "create rule \"_RETURN\" as on select to v do also select 1 as a",
        Some("rules on SELECT must have action INSTEAD SELECT"),
    ),
    (
        "create rule r as on select to v where true do instead nothing",
        Some("rules on SELECT must have action INSTEAD SELECT"),
    ),
    (
        "alter table lp1 no inherit lp",
        Some("cannot change inheritance of a partition"),
    ),
    (
        "alter table lp1 inherit lp",
        Some("cannot change inheritance of a partition"),
    ),
];

/// The relations [`TOAST_AND_ADD_COLUMN_CASES`] run over.
const TOAST_AND_ADD_COLUMN_SETUP: &str = "
    CREATE TABLE t (a int, b text);
    CREATE TABLE par (a int, b int);
    CREATE TABLE chi () INHERITS (par);
    CREATE TABLE lone (a int);
    CREATE TABLE lp (a int) PARTITION BY LIST (a);
    CREATE TABLE lp1 PARTITION OF lp FOR VALUES IN (1);
    CREATE TYPE ct AS (a int);
    CREATE TABLE tt OF ct;
    CREATE TABLE pk (a int, b int) PARTITION BY LIST (a);
    CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';";

#[test]
fn toast_tables_follow_rewrites_and_add_column_if_not_exists_skips_first() {
    // A heap rewrite (ALTER TABLE's, CLUSTER, REFRESH MATERIALIZED VIEW)
    // makes a TOAST table only if the columns then need one, so `toast.`
    // options go unchecked once none is left; TRUNCATE and a mere DROP
    // COLUMN or SET STORAGE keep it. ADD COLUMN IF NOT EXISTS skips an
    // existing column before its type, collation, default or constraints
    // are looked at. A partition key a unique constraint misses is named in
    // the DETAIL, and REINDEX (TABLESPACE ...) wants a tablespace that
    // exists. What PG runs is tried in a transaction rolled back.
    let mut db = build_db(&[("0001.sql", TOAST_AND_ADD_COLUMN_SETUP)]);
    for &(sql, expected) in TOAST_AND_ADD_COLUMN_CASES {
        match expected {
            None => {
                if let Err(err) = db.apply_sql(&format!("BEGIN; {sql}; ROLLBACK;")) {
                    panic!("{sql}\n  rejected: {err}");
                }
            }
            Some(message) => {
                let err = db.apply_sql(&format!("{sql};")).expect_err(sql);
                assert!(
                    err.to_string().starts_with(message),
                    "{sql}\n  expected: {message}\n       got: {err}"
                );
            }
        }
    }
}

#[test]
fn vacuum_full_makes_the_toast_table_anew() {
    // VACUUM FULL rewrites the heap (make_new_heap): a TOAST table only
    // the dropped column needed goes; one a remaining column needs stays.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b text); CREATE TABLE u (a int, b text, c text);",
    )]);
    db.apply_sql("ALTER TABLE t DROP COLUMN b; ALTER TABLE u DROP COLUMN b;")
        .unwrap();
    let toast_fillfactor = "unrecognized parameter \"fillfactor\"";
    let err = db
        .apply_sql("ALTER TABLE t SET (toast.fillfactor = 50);")
        .unwrap_err();
    assert!(err.to_string().starts_with(toast_fillfactor), "got: {err}");
    db.apply_sql("VACUUM FULL t;").unwrap();
    db.apply_sql("VACUUM FULL u;").unwrap();
    db.apply_sql("ALTER TABLE t SET (toast.fillfactor = 50);")
        .unwrap();
    let err = db
        .apply_sql("ALTER TABLE u SET (toast.fillfactor = 50);")
        .unwrap_err();
    assert!(err.to_string().starts_with(toast_fillfactor), "got: {err}");
}

/// `(statement, PG's error)` — `None` when PG runs it — over
/// [`TOAST_AND_ADD_COLUMN_SETUP`], each checked on PostgreSQL 18.6.
const TOAST_AND_ADD_COLUMN_CASES: &[(&str, Option<&str>)] = &[
    (
        "alter table t alter column b type int using 1; select 1",
        None,
    ),
    ("alter table t alter column b type int using 1", None),
    (
        "alter table t set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create table t2 (a int, b text); alter table t2 alter column b type int using 1; alter table t2 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "create table t2 (a int, b text); alter table t2 alter column b type int using 1, add column c text; alter table t2 set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create table t2 (a int, b text); alter table t2 drop column b; alter table t2 set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create table t2 (a int, b text); alter table t2 drop column b; alter table t2 add column c float8 default random(); alter table t2 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "create table t2 (a int, b text); alter table t2 drop column b; alter table t2 set unlogged; alter table t2 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "create table t2 (a int, b text); alter table t2 drop column b; truncate t2; alter table t2 set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create table t2 (a int, b text); alter table t2 alter column b set storage plain; alter table t2 alter column a type bigint; alter table t2 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "create table t2 (a int, b text); create index t2i on t2 (a); alter table t2 alter column b set storage plain; cluster t2 using t2i; alter table t2 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "create table t2 (a int, b text); alter table t2 alter column b set storage plain; alter table t2 set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create table t2 (a int, b text storage plain); alter table t2 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "create table t2 (a int, b text); alter table t2 alter column b type varchar(10); alter table t2 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "create materialized view m2 as select 1 as a, 'x'::text as b; alter materialized view m2 set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create table t2 (a int, b text); alter table t2 drop column b; vacuum full t2",
        Some("VACUUM cannot run inside a transaction block"),
    ),
    (
        "create table t2 (a int, b text); alter table t2 drop column b, alter column a type bigint; alter table t2 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "alter table t add column if not exists a nonexisttype",
        None,
    ),
    (
        "alter table t add column if not exists a int default nonexist",
        None,
    ),
    (
        "alter table t add column if not exists a text collate \"nonexist\"",
        None,
    ),
    (
        "alter table t add column if not exists a int references nonexist",
        None,
    ),
    (
        "alter table t add column if not exists a int check (nonexist > 0)",
        None,
    ),
    (
        "alter table t add column if not exists a int generated always as (nonexist) stored",
        None,
    ),
    (
        "alter table t add column if not exists z nonexisttype",
        Some("type \"nonexisttype\" does not exist"),
    ),
    (
        "alter table t add column if not exists z int default nonexist",
        Some("cannot use column reference in DEFAULT expression"),
    ),
    (
        "alter table only par add column if not exists a nonexisttype",
        None,
    ),
    (
        "alter table par add column if not exists a nonexisttype",
        None,
    ),
    (
        "alter table par add column if not exists a int default nonexist",
        None,
    ),
    (
        "alter table only lone add column if not exists a nonexisttype",
        None,
    ),
    (
        "alter table lp1 add column if not exists a nonexisttype",
        Some("cannot add column to a partition"),
    ),
    (
        "alter table lp1 add column if not exists a int",
        Some("cannot add column to a partition"),
    ),
    (
        "alter table lp1 add column if not exists z int",
        Some("cannot add column to a partition"),
    ),
    (
        "alter table tt add column if not exists a nonexisttype",
        Some("cannot add column to typed table"),
    ),
    (
        "alter table tt add column if not exists a int",
        Some("cannot add column to typed table"),
    ),
    (
        "alter table lp add column if not exists a nonexisttype",
        None,
    ),
    (
        "alter table only lp add column if not exists a nonexisttype",
        None,
    ),
    (
        "alter table t add column if not exists a int primary key using index tablespace nonexist",
        None,
    ),
    (
        "alter table t add column if not exists ctid int",
        Some("column name \"ctid\" conflicts with a system column name"),
    ),
    ("alter table t add column if not exists a serial", None),
    (
        "alter table pk add unique (b)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (UNIQUE constraint on table \"pk\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "alter table pk add primary key (b)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (PRIMARY KEY constraint on table \"pk\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "create unique index on pk (b)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (UNIQUE constraint on table \"pk\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "alter table pk add constraint x exclude (b with =)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (EXCLUDE constraint on table \"pk\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "create table pk2 (a int, b int, unique (b)) partition by list (a)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (UNIQUE constraint on table \"pk2\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "create table pk2 (a int, b int, primary key (b)) partition by list (a)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (PRIMARY KEY constraint on table \"pk2\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "create table pk2 (a int, b int, primary key (b)) partition by list ((a + 1))",
        Some(
            "unsupported PRIMARY KEY constraint with partition key definition (PRIMARY KEY constraints cannot be used when partition keys include expressions.",
        ),
    ),
    (
        "alter table pk add unique (b, (a))",
        Some("syntax error at or near \"(\""),
    ),
    (
        "reindex (tablespace nonexist) schema pg_temp",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "reindex (tablespace nonexist) schema public",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "reindex (tablespace nonexist) table t",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    ("reindex (tablespace pg_global) table t", None),
    (
        "reindex (tablespace nonexist) index nosuch",
        Some("tablespace \"nonexist\" does not exist"),
    ),
];

/// The relations [`REWRITE_CASES`] run over.
const REWRITE_SETUP: &str = "
    CREATE TABLE par (a int, v text); CREATE TABLE ch (b int) INHERITS (par);
    ALTER TABLE par ALTER COLUMN v SET STORAGE PLAIN;
    CREATE INDEX i ON par (a);
    CREATE TABLE t (a int, b text); CREATE INDEX ti ON t (a);
    CREATE TABLE nt (a int, b text); CREATE INDEX nti ON nt (a);
    CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a, 'x'::text AS b WITH NO DATA;
    CREATE DOMAIN dvc AS varchar(10); CREATE DOMAIN dnum AS numeric(10, 2);
    CREATE DOMAIN dbit AS bit(3);
    CREATE TABLE pk (a int, b int) PARTITION BY RANGE (a, (b + 1));
    CREATE TABLE pe (a int, b int) PARTITION BY RANGE ((a + 1), b);";

#[test]
fn rewrites_vacuum_options_and_tablespaces_follow_pg() {
    // CLUSTER rewrites the table it names (a partitioned table's leaf
    // partitions), bare CLUSTER every table with a clustered index, VACUUM
    // FULL inheritance children too — none of them a materialized view
    // never populated; ExecVacuum's options (the last FULL, PROCESS_MAIN,
    // and the combinations it refuses); a domain column's size is
    // unbounded for needs_toast_table; REINDEX's last TABLESPACE, looked up
    // after the other options; the partition key walked in order for a
    // unique or exclusion constraint; a tablespace that must exist. Each
    // case runs on a catalog of its own (VACUUM and bare CLUSTER can't run
    // in a transaction).
    for &(steps, expected) in REWRITE_CASES {
        let mut db = build_db(&[("0001.sql", REWRITE_SETUP)]);
        let steps: Vec<&str> = steps.split(" ;; ").collect();
        let (last, before) = steps.split_last().unwrap();
        for step in before {
            if let Err(err) = db.apply_sql(&format!("{step};")) {
                panic!("{step}\n  rejected: {err}");
            }
        }
        match (db.apply_sql(&format!("{last};")), expected) {
            (Ok(()), None) => {}
            (Err(err), None) => panic!("{last}\n  rejected: {err}"),
            (Ok(()), Some(message)) => panic!("{last}\n  accepted, expected: {message}"),
            (Err(err), Some(message)) => assert!(
                err.to_string().starts_with(message),
                "{last}\n  expected: {message}\n       got: {err}"
            ),
        }
    }
}

/// `(statements, PG's error)` over [`REWRITE_SETUP`]: the statements, `;;`
/// apart, run one after another, and the last fails with the error (or
/// none does) — each checked on PostgreSQL 18.6.
const REWRITE_CASES: &[(&str, Option<&str>)] = &[
    (
        "cluster par using i ;; alter table ch set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "cluster par using i ;; alter table par set (toast.fillfactor = 50)",
        None,
    ),
    (
        "alter table ch alter column v set storage plain ;; vacuum full par ;; alter table ch set (toast.fillfactor = 50)",
        None,
    ),
    (
        "alter table t alter column b set storage plain ;; alter table t cluster on ti ;; cluster ;; alter table t set (toast.fillfactor = 50)",
        None,
    ),
    (
        "alter table t alter column b set storage plain ;; alter table t cluster on ti ;; cluster verbose ;; alter table t set (toast.fillfactor = 50)",
        None,
    ),
    (
        "alter table t alter column b set storage plain ;; cluster ;; alter table t set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "alter table t alter column b set storage plain ;; vacuum (full, full false) t ;; alter table t set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "alter table t alter column b set storage plain ;; vacuum (full, process_main off) t ;; alter table t set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "alter table t alter column b set storage plain ;; vacuum (full false, full) t ;; alter table t set (toast.fillfactor = 50)",
        None,
    ),
    (
        "vacuum (full, process_toast off) t",
        Some("PROCESS_TOAST required with VACUUM FULL"),
    ),
    (
        "vacuum (full, disable_page_skipping) t",
        Some("VACUUM option DISABLE_PAGE_SKIPPING cannot be used with FULL"),
    ),
    (
        "vacuum (full, buffer_usage_limit '256kB') t",
        Some("BUFFER_USAGE_LIMIT cannot be specified for VACUUM FULL"),
    ),
    ("vacuum (full, analyze, buffer_usage_limit '256kB') t", None),
    (
        "vacuum (full, parallel 2) t",
        Some("VACUUM FULL cannot be performed in parallel"),
    ),
    ("vacuum (parallel 2) t", None),
    ("vacuum (only_database_stats, verbose)", None),
    (
        "vacuum (only_database_stats, analyze)",
        Some("ONLY_DATABASE_STATS cannot be specified with other VACUUM options"),
    ),
    (
        "vacuum (only_database_stats) t",
        Some("ONLY_DATABASE_STATS cannot be specified with a list of tables"),
    ),
    ("vacuum (only_database_stats)", None),
    (
        "vacuum (full nonsense) t",
        Some("full requires a Boolean value"),
    ),
    (
        "vacuum full t (a)",
        Some("ANALYZE option must be specified when a column list is provided"),
    ),
    ("vacuum (full, analyze) t (a)", None),
    (
        "analyze (full) t",
        Some("unrecognized ANALYZE option \"full\""),
    ),
    ("analyze (verbose, skip_locked) t", None),
    ("vacuum (foo) t", Some("unrecognized VACUUM option \"foo\"")),
    (
        "vacuum (parallel) t",
        Some("parallel option requires a value between 0 and 1024"),
    ),
    (
        "vacuum (parallel 2000) t",
        Some("parallel workers for vacuum must be between 0 and 1024"),
    ),
    (
        "vacuum (parallel 'x') t",
        Some("parallel requires an integer value"),
    ),
    (
        "vacuum (buffer_usage_limit '1kB') t",
        Some("BUFFER_USAGE_LIMIT option must be 0 or between 128 kB and 16777216 kB"),
    ),
    (
        "vacuum (buffer_usage_limit 'x') t",
        Some("BUFFER_USAGE_LIMIT option must be 0 or between 128 kB and 16777216 kB"),
    ),
    (
        "vacuum (buffer_usage_limit) t",
        Some("buffer_usage_limit requires a parameter"),
    ),
    ("vacuum (index_cleanup auto, truncate false) t", None),
    (
        "vacuum (index_cleanup maybe) t",
        Some("index_cleanup requires a Boolean value"),
    ),
    (
        "vacuum (truncate maybe) t",
        Some("truncate requires a Boolean value"),
    ),
    (
        "vacuum (process_toast maybe) t",
        Some("process_toast requires a Boolean value"),
    ),
    (
        "vacuum (only_database_stats, process_main off, process_toast off)",
        None,
    ),
    (
        "alter materialized view mv alter column b set storage plain ;; vacuum full mv ;; alter materialized view mv set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create unique index mvi on mv (a) ;; alter materialized view mv alter column b set storage plain ;; cluster mv using mvi ;; alter materialized view mv set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "alter materialized view mv alter column b set storage plain ;; refresh materialized view mv with no data ;; alter materialized view mv set (toast.fillfactor = 50)",
        None,
    ),
    (
        "create table d1 (a int, x dvc) ;; alter table d1 set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create table d2 (a int, x dnum) ;; alter table d2 set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create table d3 (a int, x dbit) ;; alter table d3 set (toast.fillfactor = 50)",
        Some("unrecognized parameter \"fillfactor\""),
    ),
    (
        "create table d4 (a int, x varchar(10)) ;; alter table d4 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "create table d5 (a int, x dvc storage plain) ;; alter table d5 set (toast.fillfactor = 50)",
        None,
    ),
    (
        "reindex (tablespace nonexist, tablespace pg_default) table t",
        None,
    ),
    (
        "reindex (tablespace pg_default, tablespace nonexist) table t",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "reindex (tablespace) table t",
        Some("tablespace requires a parameter"),
    ),
    (
        "reindex (tablespace 5) table t",
        Some("tablespace \"5\" does not exist"),
    ),
    (
        "reindex (tablespace 1.5) table t",
        Some("tablespace \"1.5\" does not exist"),
    ),
    (
        "reindex (tablespace nonexist, concurrently maybe) table t",
        Some("concurrently requires a Boolean value"),
    ),
    (
        "alter table pk add unique (b)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (UNIQUE constraint on table \"pk\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "alter table pk add unique (a)",
        Some(
            "unsupported UNIQUE constraint with partition key definition (UNIQUE constraints cannot be used when partition keys include expressions.",
        ),
    ),
    (
        "alter table pk add unique (a, b)",
        Some(
            "unsupported UNIQUE constraint with partition key definition (UNIQUE constraints cannot be used when partition keys include expressions.",
        ),
    ),
    (
        "create unique index on pk (b)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (UNIQUE constraint on table \"pk\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "alter table pe add unique (b)",
        Some(
            "unsupported UNIQUE constraint with partition key definition (UNIQUE constraints cannot be used when partition keys include expressions.",
        ),
    ),
    (
        "alter table pk add constraint x exclude (b with =)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (EXCLUDE constraint on table \"pk\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "alter table pk add constraint x exclude (a with =)",
        Some(
            "unsupported EXCLUDE constraint with partition key definition (EXCLUDE constraints cannot be used when partition keys include expressions.",
        ),
    ),
    (
        "alter table pe add constraint x exclude (b with =)",
        Some(
            "unsupported EXCLUDE constraint with partition key definition (EXCLUDE constraints cannot be used when partition keys include expressions.",
        ),
    ),
    (
        "create table pke (a int, b int) partition by range (a); alter table pke add constraint x exclude (b with =)",
        Some(
            "unique constraint on partitioned table must include all partitioning columns (EXCLUDE constraint on table \"pke\" lacks column \"a\" which is part of the partition key.",
        ),
    ),
    (
        "alter table t add column c int unique using index tablespace nonexist",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "alter table t add column c int unique using index tablespace pg_global",
        Some("only shared relations can be placed in pg_global tablespace"),
    ),
    (
        "create table u (a int unique using index tablespace nonexist)",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "create table u (a int, unique (a) using index tablespace nonexist)",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "create table u (a int, primary key (a) using index tablespace nonexist)",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "alter table t add constraint k unique (a) using index tablespace nonexist",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "create table u (a int) tablespace nonexist",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "create index on t (a) tablespace nonexist",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "alter table t set tablespace nonexist",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "alter index ti set tablespace nonexist",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "create table u tablespace nonexist as select 1",
        Some("tablespace \"nonexist\" does not exist"),
    ),
    (
        "create materialized view u tablespace nonexist as select 1",
        Some("tablespace \"nonexist\" does not exist"),
    ),
];
