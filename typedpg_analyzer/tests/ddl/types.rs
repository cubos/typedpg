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

// ── ALTER DOMAIN ────────────────────────────────────────────────────────────

#[test]
fn alter_domain_drop_not_null_makes_columns_nullable() {
    // PG 18: after DROP NOT NULL the domain's typnotnull is false.
    let db = build_db(&[(
        "0001.sql",
        "CREATE DOMAIN d AS int NOT NULL;
         CREATE TABLE t (a d);
         ALTER DOMAIN d DROP NOT NULL;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM t").unwrap(),
        vec![cn("a", domain("public", "d", int4()))],
    );
}

#[test]
fn alter_domain_set_not_null_and_named_constraints() {
    // PG 18: SET NOT NULL / ADD CONSTRAINT nn NOT NULL set typnotnull;
    // DROP CONSTRAINT d_not_null (the generated name) clears it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE DOMAIN d AS int;
         CREATE TABLE t (a d);
         ALTER DOMAIN d SET NOT NULL;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM t").unwrap(),
        vec![c("a", domain("public", "d", int4()))],
    );
    let db = build_db(&[(
        "0001.sql",
        "CREATE DOMAIN d AS int NOT NULL;
         CREATE TABLE t (a d);
         ALTER DOMAIN d DROP CONSTRAINT d_not_null;
         CREATE DOMAIN d2 AS int CONSTRAINT myname NOT NULL;
         CREATE TABLE t2 (a d2);
         ALTER DOMAIN d2 DROP CONSTRAINT myname;
         CREATE DOMAIN d3 AS int;
         CREATE TABLE t3 (a d3);
         ALTER DOMAIN d3 ADD CONSTRAINT nn NOT NULL;
         ALTER DOMAIN d3 ADD CONSTRAINT c CHECK (VALUE > 0);
         ALTER DOMAIN d3 DROP CONSTRAINT c;
         ALTER DOMAIN d3 DROP CONSTRAINT IF EXISTS zz;
         ALTER DOMAIN d3 SET DEFAULT 1;
         ALTER DOMAIN d3 DROP DEFAULT;",
    )]);
    assert_cols(
        &db.analyze("SELECT a FROM t").unwrap(),
        vec![cn("a", domain("public", "d", int4()))],
    );
    assert_cols(
        &db.analyze("SELECT a FROM t2").unwrap(),
        vec![cn("a", domain("public", "d2", int4()))],
    );
    assert_cols(
        &db.analyze("SELECT a FROM t3").unwrap(),
        vec![c("a", domain("public", "d3", int4()))],
    );
}

#[test]
fn alter_domain_errors() {
    // PG 18: 42704 type "nosuch" does not exist; 42809 t is not a domain;
    // 42704 constraint "zz" of domain "d" does not exist.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "ALTER DOMAIN nosuch SET NOT NULL;")]),
        DdlError::TypeNotFound(_),
        "type \"nosuch\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (a int); ALTER DOMAIN t SET NOT NULL;",
        )]),
        DdlError::Parse(_),
        "t is not a domain",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE DOMAIN d AS int; ALTER DOMAIN d DROP CONSTRAINT zz;",
        )]),
        DdlError::TypeNotFound(_),
        "constraint \"zz\" of domain \"d\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE DOMAIN d AS int; ALTER DOMAIN d ADD CONSTRAINT c2 CHECK (VALUE + 1);",
        )]),
        DdlError::UnsupportedDdl(_),
        "argument of CHECK must be type boolean, not type integer",
    );
}

#[test]
fn create_domain_check_must_be_boolean() {
    // PG 18: 42804 argument of CHECK must be type boolean, not type integer.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE DOMAIN d AS int CHECK (VALUE + 1);")]),
        DdlError::UnsupportedDdl(_),
        "argument of CHECK must be type boolean, not type integer",
    );
    build_db(&[(
        "0001.sql",
        "CREATE DOMAIN pos AS int CHECK (VALUE > 0) CHECK (VALUE < 100);",
    )]);
}

// ── CREATE TYPE ... AS RANGE: multirange + constructors ─────────────────────

#[test]
fn create_range_creates_the_multirange_type() {
    // PG 18: floatrange → floatmultirange, fr → fr_multirange,
    // multirange_type_name overrides.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TYPE floatrange AS RANGE (subtype = float8, subtype_diff = float8mi);
         CREATE TYPE fr AS RANGE (subtype = float8);
         CREATE TYPE textrng AS RANGE (subtype = text, multirange_type_name = tmr);
         CREATE TABLE t (r floatrange, m floatmultirange, m2 fr_multirange, m3 tmr);",
    )]);
    let info = db.analyze("SELECT * FROM t").unwrap();
    let names: Vec<String> = info
        .columns
        .iter()
        .map(|c| c.pg_type.cast_name().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        vec![
            "public.floatrange",
            "public.floatmultirange",
            "public.fr_multirange",
            "public.tmr"
        ]
    );
}

#[test]
fn create_range_creates_the_constructor_functions() {
    // PG 18: fr(...) returns fr; multirange(r) returns floatmultirange;
    // fr_multirange() / fr_multirange(fr) / fr_multirange(VARIADIC) exist.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TYPE floatrange AS RANGE (subtype = float8);
         CREATE TYPE fr AS RANGE (subtype = float8);
         CREATE TABLE t (r floatrange);",
    )]);
    let info = db
        .analyze(
            "SELECT fr(1.0::float8, 2.0::float8) AS a, fr(1, 2, '[]') AS b,
                    multirange(r) AS c, fr_multirange() AS d,
                    fr_multirange(fr(1, 2)) AS e, fr_multirange(fr(1, 2), fr(3, 4)) AS f
             FROM t",
        )
        .unwrap();
    let names: Vec<String> = info
        .columns
        .iter()
        .map(|c| c.pg_type.cast_name().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        vec![
            "public.fr",
            "public.fr",
            "public.floatmultirange",
            "public.fr_multirange",
            "public.fr_multirange",
            "public.fr_multirange"
        ]
    );
}

#[test]
fn create_range_requires_a_subtype() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TYPE r AS RANGE (subtype_diff = float8mi);"
        )]),
        DdlError::Parse(_),
        "type attribute \"subtype\" is required",
    );
}

// ── ALTER TYPE ... ATTRIBUTE on composite types ────────────────────────────

#[test]
fn composite_type_attributes_follow_alter_type() {
    // PG 18: ADD / DROP / RENAME ATTRIBUTE are allowed even while a table
    // uses the type; ALTER ATTRIBUTE ... TYPE is not.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TYPE c AS (x int);
         CREATE TABLE tc (v c);
         ALTER TYPE c ADD ATTRIBUTE z int;
         ALTER TYPE c RENAME ATTRIBUTE x TO w;",
    )]);
    let info = db.analyze("SELECT (v).w, (v).z FROM tc").unwrap();
    assert_eq!(info.columns.len(), 2);
    let err = try_apply(&[(
        "0001.sql",
        "CREATE TYPE c AS (x int); CREATE TABLE tc (v c);
         ALTER TYPE c ALTER ATTRIBUTE x TYPE text;",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot alter type \"c\" because column \"tc.v\" uses it"),
        "{err}"
    );
    build_db(&[(
        "0001.sql",
        "CREATE TYPE c AS (x int); ALTER TYPE c ALTER ATTRIBUTE x TYPE text;",
    )]);
}

// ── Enum labels (EnumValuesCreate / AddEnumLabel / RenameEnumLabel) ─────────

#[test]
fn enum_label_changes_follow_pg() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TYPE e AS ENUM ('a', 'b');
         ALTER TYPE e RENAME VALUE 'a' TO 'z';
         ALTER TYPE e ADD VALUE IF NOT EXISTS 'b' BEFORE 'nosuch';",
    )]);
    let e = db.resolve_type_by_name(None, "e").unwrap();
    assert_eq!(db.enum_labels_of(e.oid), vec!["z", "b"]);
    for (stmt, msg) in [
        (
            "ALTER TYPE e RENAME VALUE 'a' TO 'b';",
            "enum label \"b\" already exists",
        ),
        (
            "ALTER TYPE e RENAME VALUE 'q' TO 'z';",
            "\"q\" is not an existing enum label",
        ),
        (
            "ALTER TYPE e ADD VALUE 'c' BEFORE 'zz';",
            "\"zz\" is not an existing enum label",
        ),
        ("ALTER TYPE t ADD VALUE 'x';", "t is not an enum"),
        (
            "ALTER TYPE nosuch ADD VALUE 'x';",
            "type \"nosuch\" does not exist",
        ),
        (
            "CREATE TYPE e2 AS ENUM ('a', 'a');",
            "duplicate key value violates unique constraint \"pg_enum_typid_label_index\"",
        ),
    ] {
        let err = try_apply(&[
            (
                "0001.sql",
                "CREATE TYPE e AS ENUM ('a', 'b'); CREATE TABLE t (a int);",
            ),
            ("0002.sql", stmt),
        ])
        .expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
}

// ── Physical layout (typlen, typbyval, typalign, typstorage, typsubscript) ──

/// `(typtype, typlen, typbyval, typalign, typstorage, subscript handler)`.
fn layout(
    snap: &PgCatalog,
    name: &str,
) -> (TypType, i16, bool, TypAlign, TypStorage, Option<String>) {
    let t = snap
        .resolve_type_by_name(None, name)
        .unwrap_or_else(|| panic!("type {name}"));
    let handler = t
        .typsubscript
        .and_then(|h| snap.pg_proc().get(&h))
        .map(|p| p.proname.clone());
    (
        t.typtype,
        t.typlen,
        t.typbyval,
        t.typalign,
        t.typstorage,
        handler,
    )
}

#[test]
fn created_types_get_postgres_physical_layout() {
    use TypAlign::*;
    use TypStorage::*;
    // Values read from pg_type on PostgreSQL 18 after the same DDL.
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE e AS ENUM ('a');
         CREATE TYPE c AS (a int, b float8);
         CREATE DOMAIN d AS int4[];
         CREATE DOMAIN dt AS text;
         CREATE TYPE r AS RANGE (subtype = float8);
         CREATE TYPE ri AS RANGE (subtype = int2);
         CREATE TABLE t (a int);
         CREATE VIEW v AS SELECT 1 AS x;",
    )]);
    let array = Some("array_subscript_handler".to_owned());
    for (name, expected) in [
        ("e", (TypType::Enum, 4, true, Int, Plain, None)),
        (
            "_e",
            (TypType::Base, -1, false, Int, Extended, array.clone()),
        ),
        ("c", (TypType::Composite, -1, false, Double, Extended, None)),
        (
            "_c",
            (TypType::Base, -1, false, Double, Extended, array.clone()),
        ),
        // A domain stores like its base, but isn't subscripted by itself.
        ("d", (TypType::Domain, -1, false, Int, Extended, None)),
        ("dt", (TypType::Domain, -1, false, Int, Extended, None)),
        ("r", (TypType::Range, -1, false, Double, Extended, None)),
        (
            "r_multirange",
            (TypType::Multirange, -1, false, Double, Extended, None),
        ),
        (
            "_r",
            (TypType::Base, -1, false, Double, Extended, array.clone()),
        ),
        ("ri", (TypType::Range, -1, false, Int, Extended, None)),
        (
            "ri_multirange",
            (TypType::Multirange, -1, false, Int, Extended, None),
        ),
        ("t", (TypType::Composite, -1, false, Double, Extended, None)),
        ("v", (TypType::Composite, -1, false, Double, Extended, None)),
    ] {
        assert_eq!(layout(&snap, name), expected, "{name}");
    }
}

#[test]
fn shell_types_are_undefined_pseudo_types_until_fully_created() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TYPE sh;
         CREATE TYPE f1;
         CREATE FUNCTION f1_in(cstring) RETURNS f1 LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
         CREATE FUNCTION f1_out(f1) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
         CREATE TYPE f1 (INPUT = f1_in, OUTPUT = f1_out);
         CREATE TYPE f2;
         CREATE FUNCTION f2_in(cstring) RETURNS f2 LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
         CREATE FUNCTION f2_out(f2) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
         CREATE TYPE f2 (INPUT = f2_in, OUTPUT = f2_out, INTERNALLENGTH = 4, PASSEDBYVALUE, ALIGNMENT = int4);
         CREATE TYPE f3;
         CREATE FUNCTION f3_in(cstring) RETURNS f3 LANGUAGE internal IMMUTABLE STRICT AS 'float8in';
         CREATE FUNCTION f3_out(f3) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'float8out';
         CREATE TYPE f3 (INPUT = f3_in, OUTPUT = f3_out, INTERNALLENGTH = 8, PASSEDBYVALUE, ALIGNMENT = double);
         CREATE TYPE f4;
         CREATE FUNCTION f4_in(cstring) RETURNS f4 LANGUAGE internal IMMUTABLE STRICT AS 'int8in';
         CREATE FUNCTION f4_out(f4) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int8out';
         CREATE TYPE f4 (INPUT = f4_in, OUTPUT = f4_out, LIKE = int8);",
    )]);
    use TypAlign::*;
    use TypStorage::*;
    let sh = snap.resolve_type_by_name(None, "sh").unwrap();
    assert!(!sh.typisdefined);
    assert_eq!(
        layout(&snap, "sh"),
        (TypType::Pseudo, 4, true, Int, Plain, None)
    );
    assert!(
        snap.resolve_type_by_name(None, "_sh").is_none(),
        "a shell has no array"
    );
    for (name, expected) in [
        ("f1", (TypType::Base, -1, false, Int, Plain, None)),
        ("f2", (TypType::Base, 4, true, Int, Plain, None)),
        ("f3", (TypType::Base, 8, true, Double, Plain, None)),
        ("f4", (TypType::Base, 8, true, Double, Plain, None)),
    ] {
        assert_eq!(layout(&snap, name), expected, "{name}");
        assert!(snap.resolve_type_by_name(None, name).unwrap().typisdefined);
    }
    assert_eq!(layout(&snap, "_f1").3, Int);
    assert_eq!(layout(&snap, "_f3").3, Double);
}

#[test]
fn subscript_handlers_are_resolved_like_define_type() {
    let shell = "CREATE TYPE f5;
         CREATE FUNCTION f5_in(cstring) RETURNS f5 LANGUAGE internal IMMUTABLE STRICT AS 'textin';
         CREATE FUNCTION f5_out(f5) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'textout';";
    for (definition, message) in [
        (
            "CREATE TYPE f5 (INPUT = f5_in, OUTPUT = f5_out, SUBSCRIPT = nosuch_handler);",
            "function nosuch_handler(internal) does not exist",
        ),
        (
            "CREATE TYPE f5 (INPUT = f5_in, OUTPUT = f5_out, SUBSCRIPT = array_subscript_handler);",
            "user-defined types cannot use subscripting function array_subscript_handler",
        ),
    ] {
        let err = try_apply(&[("0001.sql", shell), ("0002.sql", definition)]).unwrap_err();
        assert!(err.to_string().starts_with(message), "{definition}: {err}");
    }
    let snap = build(&[("0001.sql", "CREATE EXTENSION hstore;")]);
    assert_eq!(
        layout(&snap, "hstore"),
        (
            TypType::Base,
            -1,
            false,
            TypAlign::Int,
            TypStorage::Extended,
            Some("hstore_subscript_handler".to_owned())
        )
    );
}
