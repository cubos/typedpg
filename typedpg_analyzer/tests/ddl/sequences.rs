//! CREATE / ALTER / DROP SEQUENCE (standalone, not via SERIAL) and the
//! sequence-manipulation functions (`nextval` / `currval` / `lastval` /
//! `setval`). Sequences are registered as `pg_class` rows with
//! `relkind = Sequence` — see `typedpg_analyzer/src/ddl/sequences.rs`.

use crate::common::*;

// ── CREATE SEQUENCE ────────────────────────────────────────────────────────

#[test]
fn create_sequence_basic_registers_pg_class() {
    let snap = build(&[("0001.sql", "CREATE SEQUENCE my_seq;")]);

    let seq = snap.resolve_table(None, "my_seq").unwrap();
    assert_eq!(seq.relname, "my_seq");
    assert_eq!(seq.relkind, RelKind::Sequence);
    assert_eq!(snap.namespace_name(seq.relnamespace), Some("public"));
}

#[test]
fn create_sequence_with_options_is_accepted() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SEQUENCE s START WITH 100 INCREMENT BY 5 \
         MINVALUE 0 MAXVALUE 1000 CACHE 10 CYCLE;",
    )]);

    let seq = snap.resolve_table(None, "s").unwrap();
    assert_eq!(seq.relkind, RelKind::Sequence);
}

#[test]
fn create_sequence_in_explicit_schema() {
    let snap = build(&[("0001.sql", "CREATE SCHEMA app; CREATE SEQUENCE app.s;")]);

    let seq = snap.resolve_table(Some("app"), "s").unwrap();
    assert_eq!(seq.relkind, RelKind::Sequence);
    assert_eq!(snap.namespace_name(seq.relnamespace), Some("app"));
    assert!(snap.resolve_table(Some("public"), "s").is_none());
}

#[test]
fn create_sequence_if_not_exists_is_silent_on_duplicate() {
    let snap = build(&[
        ("0001.sql", "CREATE SEQUENCE s;"),
        ("0002.sql", "CREATE SEQUENCE IF NOT EXISTS s;"),
    ]);

    let seq = snap.resolve_table(None, "s").unwrap();
    assert_eq!(seq.relkind, RelKind::Sequence);
    let public_oid = snap.namespace_oid("public").unwrap();
    let count = snap
        .pg_class()
        .values()
        .filter(|c| {
            c.relnamespace == public_oid && c.relname == "s" && c.relkind == RelKind::Sequence
        })
        .count();
    assert_eq!(count, 1, "IF NOT EXISTS must not register a duplicate row");
}

#[test]
fn create_sequence_duplicate_without_if_not_exists_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE SEQUENCE seq;"),
        ("0002.sql", "CREATE SEQUENCE seq;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "relation \"seq\" already exists"
    );
}

// ── DROP SEQUENCE ──────────────────────────────────────────────────────────

#[test]
fn drop_sequence_existing_removes_it() {
    let snap = build(&[
        ("0001.sql", "CREATE SEQUENCE s;"),
        ("0002.sql", "DROP SEQUENCE s;"),
    ]);

    assert!(snap.resolve_table(None, "s").is_none());
}

#[test]
fn drop_sequence_missing_errors() {
    let result = try_apply(&[("0001.sql", "DROP SEQUENCE missing;")]);

    assert_ddl_err!(
        result,
        DdlError::TableNotFound(_),
        "sequence \"missing\" does not exist"
    );
}

#[test]
fn drop_sequence_if_exists_missing_is_silent() {
    let snap = build(&[("0001.sql", "DROP SEQUENCE IF EXISTS missing;")]);
    assert!(snap.resolve_table(None, "missing").is_none());
}

#[test]
fn drop_sequence_against_table_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "DROP SEQUENCE t;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::TableNotFound(_),
        "\"t\" is not a sequence"
    );
}

// ── ALTER SEQUENCE ─────────────────────────────────────────────────────────

#[test]
fn alter_sequence_with_options_on_existing() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SEQUENCE s; ALTER SEQUENCE s RESTART WITH 100 INCREMENT BY 2;",
    )]);

    let seq = snap.resolve_table(None, "s").unwrap();
    assert_eq!(seq.relkind, RelKind::Sequence);
}

#[test]
fn alter_sequence_missing_errors() {
    let result = try_apply(&[("0001.sql", "ALTER SEQUENCE missing RESTART WITH 1;")]);

    assert_ddl_err!(
        result,
        DdlError::TableNotFound(_),
        "relation \"missing\" does not exist"
    );
}

#[test]
fn alter_sequence_if_exists_missing_is_silent() {
    let _snap = build(&[(
        "0001.sql",
        "ALTER SEQUENCE IF EXISTS missing RESTART WITH 1;",
    )]);
}

#[test]
fn alter_sequence_rename_to() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SEQUENCE s; ALTER SEQUENCE s RENAME TO s2;",
    )]);

    assert!(snap.resolve_table(None, "s").is_none());
    let renamed = snap.resolve_table(None, "s2").unwrap();
    assert_eq!(renamed.relkind, RelKind::Sequence);
}

#[test]
fn alter_sequence_set_schema() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA app;
         CREATE SEQUENCE s;
         ALTER SEQUENCE s SET SCHEMA app;",
    )]);

    assert!(snap.resolve_table(Some("public"), "s").is_none());
    let moved = snap.resolve_table(Some("app"), "s").unwrap();
    assert_eq!(moved.relkind, RelKind::Sequence);
    assert_eq!(snap.namespace_name(moved.relnamespace), Some("app"));
}

// ── Sequence functions: nextval / currval / lastval / setval ───────────────

#[test]
fn nextval_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT nextval('s')").unwrap();
    assert_cols(&info, vec![c("nextval", int8())]);
}

#[test]
fn currval_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT currval('s')").unwrap();
    assert_cols(&info, vec![c("currval", int8())]);
}

#[test]
fn lastval_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT lastval()").unwrap();
    assert_cols(&info, vec![c("lastval", int8())]);
}

#[test]
fn setval_two_args_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT setval('s', 1)").unwrap();
    assert_cols(&info, vec![c("setval", int8())]);
}

#[test]
fn setval_three_args_returns_int8_not_null() {
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);

    let info = db.analyze("SELECT setval('s', 1, true)").unwrap();
    assert_cols(&info, vec![c("setval", int8())]);
}

// ── Implicit sequences (serial, identity, OWNED BY) ────────────────────────

#[test]
fn serial_columns_are_not_null() {
    // PG 18: attnotnull = t for serial, bigserial and smallserial.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a serial, b bigserial, c smallserial);",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM t").unwrap(),
        vec![c("a", int4()), c("b", int8()), c("c", int2())],
    );
}

#[test]
fn serial_and_identity_columns_create_their_sequences() {
    // PG 18: t_id_seq / t_d_seq exist and can be altered and read.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (id serial, d int GENERATED BY DEFAULT AS IDENTITY);
         ALTER SEQUENCE t_id_seq RESTART WITH 100;
         ALTER SEQUENCE t_d_seq RESTART WITH 5;",
    )]);
    assert_cols(
        &db.analyze("SELECT last_value FROM t_id_seq").unwrap(),
        vec![c("last_value", int8())],
    );
    assert_cols(
        &db.analyze("SELECT * FROM t_d_seq").unwrap(),
        vec![
            c("last_value", int8()),
            c("log_cnt", int8()),
            c("is_called", bool_ty()),
        ],
    );
}

#[test]
fn serial_sequence_name_avoids_existing_relations() {
    // PG 18 ChooseRelationName: t2_x_seq taken → t2_x_seq1.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t2_x_seq (a int); CREATE TABLE t2 (x serial);",
    )]);
    db.analyze("SELECT last_value FROM t2_x_seq1").unwrap();
}

#[test]
fn owned_sequences_are_dropped_with_their_table_or_column() {
    // PG 18: dropping the owner removes the sequence, so its name is free.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (id serial, x int);
         CREATE SEQUENCE s OWNED BY t.x;
         DROP TABLE t;
         CREATE SEQUENCE s;
         CREATE SEQUENCE t_id_seq;
         CREATE TABLE u (a serial, b int GENERATED ALWAYS AS IDENTITY);
         ALTER TABLE u DROP COLUMN a;
         ALTER TABLE u ALTER COLUMN b DROP IDENTITY;
         CREATE SEQUENCE u_a_seq;
         CREATE SEQUENCE u_b_seq;",
    )]);
}

#[test]
fn add_column_serial_and_identity_create_sequences() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE v (a int);
         ALTER TABLE v ADD COLUMN b serial;
         ALTER TABLE v ADD COLUMN c int GENERATED ALWAYS AS IDENTITY;
         ALTER TABLE v ALTER a SET NOT NULL;
         ALTER TABLE v ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY;",
    )]);
    for seq in ["v_a_seq", "v_b_seq", "v_c_seq"] {
        db.analyze(&format!("SELECT last_value FROM {seq}"))
            .unwrap();
    }
}

#[test]
fn add_identity_requires_not_null() {
    // PG 18: ERROR 55000.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE v (a int); ALTER TABLE v ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY;",
        )]),
        DdlError::Parse(_),
        "column \"a\" of relation \"v\" must be declared NOT NULL before identity can be added",
    );
}

#[test]
fn sequence_columns_are_visible() {
    // PG 18: SELECT * FROM s → last_value bigint, log_cnt bigint, is_called boolean.
    let db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);
    assert_cols(
        &db.analyze("SELECT * FROM s").unwrap(),
        vec![
            c("last_value", int8()),
            c("log_cnt", int8()),
            c("is_called", bool_ty()),
        ],
    );
}
