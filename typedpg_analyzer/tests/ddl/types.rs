//! CREATE TYPE — enum, composite, range, domain — and ALTER ENUM
//! (ADD VALUE, ADD VALUE IF NOT EXISTS, BEFORE/AFTER).

use crate::common::*;

// ── CREATE DOMAIN ───────────────────────────────────────────────────────────

#[test]
fn create_domain() {
    let snap = build(&[("0001.sql", "CREATE DOMAIN email AS TEXT;")]);

    let te = snap.resolve_type_by_name(None, "email").unwrap();
    assert_eq!(te.typtype, TypType::Domain);
    let base = snap.get_type(te.typbasetype.unwrap()).unwrap();
    assert_eq!(base.typname, "text");

    // Array type.
    assert!(
        snap.resolve_type_by_name(Some("public"), "_email")
            .is_some()
    );
}

// ── CREATE TYPE AS ENUM ─────────────────────────────────────────────────────

#[test]
fn create_enum() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
    )]);

    let te = snap.resolve_type_by_name(None, "mood").unwrap();
    assert_eq!(te.typtype, TypType::Enum);
    let labels = snap.enum_labels_of(te.oid);
    assert_eq!(labels, vec!["sad", "ok", "happy"]);
}

#[test]
fn alter_enum_add_value() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
        ),
        (
            "0002.sql",
            "ALTER TYPE mood ADD VALUE 'ecstatic' AFTER 'happy';",
        ),
    ]);

    let te = snap.resolve_type_by_name(None, "mood").unwrap();
    assert_eq!(te.typtype, TypType::Enum);
    let labels = snap.enum_labels_of(te.oid);
    assert_eq!(labels, vec!["sad", "ok", "happy", "ecstatic"]);
}

#[test]
fn alter_enum_add_value_before() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
        ),
        (
            "0002.sql",
            "ALTER TYPE mood ADD VALUE 'anxious' BEFORE 'sad';",
        ),
    ]);

    let te = snap.resolve_type_by_name(None, "mood").unwrap();
    assert_eq!(te.typtype, TypType::Enum);
    let labels = snap.enum_labels_of(te.oid);
    assert_eq!(labels, vec!["anxious", "sad", "ok", "happy"]);
}

// ── CREATE TYPE AS (composite) ──────────────────────────────────────────────

#[test]
fn create_composite_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE address AS (
            street TEXT,
            city TEXT,
            zip TEXT
        );",
    )]);

    let te = snap.resolve_type_by_name(None, "address").unwrap();
    assert_eq!(te.typtype, TypType::Composite);
    let fields = snap.composite_fields_of(te.oid);
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].attname, "street");
    assert_eq!(fields[1].attname, "city");
    assert_eq!(fields[2].attname, "zip");
}

#[test]
fn composite_type_field_types_resolved() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE address AS (
            street TEXT,
            city TEXT,
            zip INT
        );",
    )]);

    let te = snap.resolve_type_by_name(None, "address").unwrap();
    assert_eq!(te.typtype, TypType::Composite);
    let fields = snap.composite_fields_of(te.oid);
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    assert_eq!(fields[0].atttypid, text_oid);
    assert_eq!(fields[1].atttypid, text_oid);
    assert_eq!(fields[2].atttypid, int4_oid);
}

// ── CREATE TYPE AS RANGE ────────────────────────────────────────────────────

#[test]
fn create_range_type_with_subtype() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE floatrange AS RANGE (subtype = float8);",
    )]);

    let te = snap.resolve_type_by_name(None, "floatrange").unwrap();
    assert_eq!(te.typtype, TypType::Range);
    let rng = snap.pg_type();
    let _ = rng;
    let float8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "float8")
        .unwrap()
        .oid;
    assert_eq!(snap.pg_type().get(&te.oid).map(|_| ()), Some(()));
    // Subtype lives in pg_range, keyed by rngtypid.
    let pg_range_subtype = {
        // We don't have a public pg_range() accessor, but we can use to_seed().
        let seed = snap.to_seed();
        seed.pg_range
            .iter()
            .find(|r| r.rngtypid == te.oid)
            .map(|r| r.rngsubtype)
    };
    assert_eq!(pg_range_subtype, Some(float8_oid));
}

// ── User-defined types as column types ─────────────────────────────────────

#[test]
fn enum_as_column_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE status AS ENUM ('active', 'inactive');
         CREATE TABLE t (id INT NOT NULL, s status NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let s_col = attrs.iter().find(|c| c.attname == "s").unwrap();
    let status_oid = snap.resolve_type_by_name(None, "status").unwrap().oid;
    assert_eq!(s_col.atttypid, status_oid);
}

#[test]
fn domain_as_column_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE DOMAIN email AS TEXT;
         CREATE TABLE t (id INT NOT NULL, contact email NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let contact = attrs.iter().find(|c| c.attname == "contact").unwrap();
    let email_oid = snap.resolve_type_by_name(None, "email").unwrap().oid;
    assert_eq!(contact.atttypid, email_oid);
}

#[test]
fn enum_array_as_column_type_is_array_kind() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE role AS ENUM ('admin', 'user', 'guest');
         CREATE TABLE t (id INT NOT NULL, roles role[] NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let roles = attrs.iter().find(|c| c.attname == "roles").unwrap();
    assert_ne!(roles.atttypid.get(), 0);

    let type_entry = snap.get_type(roles.atttypid).unwrap();
    assert_eq!(
        type_entry.typcategory,
        TypCategory::Array,
        "role[] should be an Array type, got {:?}",
        type_entry.typcategory
    );
}

// ── Duplicates ─────────────────────────────────────────────────────────────

#[test]
fn create_enum_duplicate_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TYPE mood AS ENUM ('happy', 'sad');"),
        ("0002.sql", "CREATE TYPE mood AS ENUM ('angry');"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "type \"mood\" already exists"
    );
}

#[test]
fn create_composite_duplicate_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TYPE point2d AS (x float8, y float8);
         CREATE TYPE point2d AS (a int, b int);",
    )]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "type \"point2d\" already exists"
    );
}

#[test]
fn create_range_duplicate_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TYPE floatrange AS RANGE (subtype = float8);
         CREATE TYPE floatrange AS RANGE (subtype = float8);",
    )]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "type \"floatrange\" already exists"
    );
}

// ── ALTER TYPE ADD VALUE edge cases ────────────────────────────────────────

#[test]
fn alter_enum_add_duplicate_value_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TYPE mood AS ENUM ('happy', 'sad');"),
        ("0002.sql", "ALTER TYPE mood ADD VALUE 'happy';"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "enum label \"happy\" already exists"
    );
}

#[test]
fn alter_enum_add_value_if_not_exists_on_missing_type_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "ALTER TYPE nonexistent ADD VALUE IF NOT EXISTS 'x';",
    )]);

    assert_ddl_err!(
        result,
        DdlError::TypeNotFound(_),
        "type \"nonexistent\" does not exist"
    );
}

#[test]
fn create_domain_with_unknown_base_type_is_rejected() {
    // PG 18: ERROR 42704 type "nosuchtype" does not exist.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE DOMAIN d AS nosuchtype;")]),
        DdlError::TypeNotFound(_),
        "type \"nosuchtype\" does not exist",
    );
}
