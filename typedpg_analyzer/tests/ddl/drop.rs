//! DROP TABLE / COLUMN / TYPE / SCHEMA / EXTENSION / FUNCTION / OPERATOR,
//! with and without CASCADE. Transitive dependency cascades, dependency
//! errors without CASCADE, IF EXISTS semantics.

use crate::common::*;

// ── DROP TABLE ──────────────────────────────────────────────────────────────

#[test]
fn drop_table() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "DROP TABLE t;"),
    ]);

    assert!(snap.resolve_table(None, "t").is_none());
    // Composite and array types should also be removed.
    assert!(snap.resolve_type_by_name(Some("public"), "t").is_none());
    assert!(snap.resolve_type_by_name(Some("public"), "_t").is_none());
}

#[test]
fn drop_table_if_exists_no_error() {
    let snap = build(&[("0001.sql", "DROP TABLE IF EXISTS nonexistent;")]);
    assert!(snap.resolve_table(None, "nonexistent").is_none());
}

// ── DROP TYPE ───────────────────────────────────────────────────────────────

#[test]
fn drop_type() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');",
        ),
        ("0002.sql", "DROP TYPE mood;"),
    ]);

    assert!(snap.resolve_type_by_name(None, "mood").is_none());
    assert!(snap.resolve_type_by_name(Some("public"), "_mood").is_none());
}

// ── DROP FUNCTION / AGGREGATE with CASCADE ─────────────────────────────────

#[test]
fn drop_function_cascade_accepted() {
    // DROP FUNCTION ... CASCADE is a syntactic valid form. It must parse
    // and execute without erroring.
    let _snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION add_one(x int) RETURNS int AS 'SELECT $1 + 1' LANGUAGE SQL;
         DROP FUNCTION add_one(int) CASCADE;",
    )]);
}

#[test]
fn drop_aggregate_cascade_accepted() {
    let _snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION sum_sfunc(state int, val int) RETURNS int AS 'SELECT $1 + $2' LANGUAGE SQL;
         CREATE AGGREGATE my_total(int) (SFUNC = sum_sfunc, STYPE = int);
         DROP AGGREGATE my_total(int) CASCADE;",
    )]);
}

// ── DROP TABLE ... CASCADE through transitive views ────────────────────────

#[test]
fn drop_table_cascade_transitive_views() {
    // Table t → view v1 → view v2. CASCADE must chase the whole chain.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v1 AS SELECT id FROM t;
             CREATE VIEW v2 AS SELECT id FROM v1;",
        ),
        ("0002.sql", "DROP TABLE t CASCADE;"),
    ]);

    assert!(snap.resolve_table(None, "t").is_none());
    assert!(snap.resolve_table(None, "v1").is_none());
    assert!(
        snap.resolve_table(None, "v2").is_none(),
        "transitive view v2 should also be dropped by CASCADE"
    );
}

// ── DROP TYPE with dependent table column ──────────────────────────────────

#[test]
fn drop_type_with_dependent_column_errors() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TYPE status AS ENUM ('a', 'b');
             CREATE TABLE t (id INT NOT NULL, s status NOT NULL);",
        ),
        ("0002.sql", "DROP TYPE status;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cannot drop type status because other objects depend on it (table(s) public.t depend on this type)"
    );
}

// ── DROP FUNCTION: overload-safe ──────────────────────────────────────────

#[test]
fn drop_function_removes_only_matching_overload() {
    // DROP FUNCTION foo(INT) must leave foo(TEXT) intact. Historically we
    // removed the whole name bucket — this guards against that regression.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
             CREATE FUNCTION foo(x TEXT) RETURNS TEXT AS $$ SELECT x $$ LANGUAGE sql;",
        ),
        ("0002.sql", "DROP FUNCTION foo(INT);"),
    ]);

    let fns = snap.find_functions(None, "foo");
    assert_eq!(
        fns.len(),
        1,
        "only the INT overload should be dropped, foo(TEXT) must survive",
    );
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    assert_eq!(
        fns[0].prorettype, text_oid,
        "remaining overload should be foo(TEXT)",
    );
}

// ── ALTER TABLE DROP COLUMN ───────────────────────────────────────────────

#[test]
fn drop_column_nonexistent_without_if_exists_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL, name TEXT);"),
        ("0002.sql", "ALTER TABLE t DROP COLUMN ghost;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::Parse(_),
        "column \"ghost\" of relation \"t\" does not exist"
    );
}

#[test]
fn drop_column_if_exists_on_nonexistent_is_noop() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL, name TEXT);"),
        (
            "0002.sql",
            "ALTER TABLE t DROP COLUMN IF EXISTS nonexistent;",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(snap.attributes_of(table.oid).len(), 2, "columns unchanged");
}

// ── DROP EXTENSION removes its provided objects ────────────────────────────

#[test]
fn drop_extension_removes_its_functions() {
    // After DROP EXTENSION, the functions registered by that extension must
    // be gone from the snapshot so downstream queries that reference them
    // surface as unresolved.
    let snap = build(&[
        ("0001.sql", "CREATE EXTENSION \"uuid-ossp\";"),
        ("0002.sql", "DROP EXTENSION \"uuid-ossp\";"),
    ]);

    let fns = snap.find_functions(None, "uuid_generate_v4");
    assert!(
        fns.is_empty(),
        "uuid_generate_v4 must not exist after DROP EXTENSION",
    );
}

// ── DROP dependency checks: functions and sequence defaults ─────────────────

#[test]
fn drop_type_cascade_drops_functions_using_it() {
    // PG 18: NOTICE drop cascades to function fe(e); fe(NULL) no longer
    // resolves. Without CASCADE the DROP is refused.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TYPE e AS ENUM ('a');
         CREATE FUNCTION fe(e) RETURNS int LANGUAGE sql AS 'select 1';
         DROP TYPE e CASCADE;",
    )]);
    let err = db.analyze("SELECT fe(NULL)").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("function fe(unknown) does not exist"),
        "{err}"
    );
    let err = try_apply(&[(
        "0001.sql",
        "CREATE TYPE e AS ENUM ('a');
         CREATE FUNCTION fe(e) RETURNS int LANGUAGE sql AS 'select 1';
         DROP TYPE e;",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot drop type e because other objects depend on it"),
        "{err}"
    );
}

#[test]
fn drop_table_whose_row_type_a_function_uses_is_refused() {
    // PG 18: 2BP01 cannot drop table tt because other objects depend on it.
    let err = try_apply(&[(
        "0001.sql",
        "CREATE TABLE tt (a int);
         CREATE FUNCTION ft(tt) RETURNS int LANGUAGE sql AS 'select 1';
         DROP TABLE tt;",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot drop table tt because other objects depend on it"),
        "{err}"
    );
    build_db(&[(
        "0001.sql",
        "CREATE TABLE tt (a int);
         CREATE FUNCTION ft(tt) RETURNS int LANGUAGE sql AS 'select 1';
         DROP TABLE tt CASCADE;
         CREATE FUNCTION ft(int) RETURNS int LANGUAGE sql AS 'select 1';",
    )]);
}

#[test]
fn drop_sequence_used_by_a_default_is_refused() {
    // PG 18: 2BP01 cannot drop sequence s because other objects depend on
    // it (the column default); CASCADE drops just the default.
    for setup in [
        "CREATE SEQUENCE s; CREATE TABLE ts (a int DEFAULT nextval('s'));",
        "CREATE SEQUENCE s; CREATE TABLE ts (a int); ALTER TABLE ts ALTER a SET DEFAULT nextval('s'::regclass);",
        "CREATE TABLE ts (a serial); ALTER SEQUENCE ts_a_seq RENAME TO s;",
    ] {
        let err =
            try_apply(&[("0001.sql", setup), ("0002.sql", "DROP SEQUENCE s;")]).expect_err(setup);
        assert!(
            err.to_string()
                .starts_with("cannot drop sequence s because other objects depend on it"),
            "{setup}\n  got: {err}"
        );
    }
    let db = build_db(&[(
        "0001.sql",
        "CREATE SEQUENCE s; CREATE TABLE ts (a int NOT NULL DEFAULT nextval('s'));
         DROP SEQUENCE s CASCADE;",
    )]);
    // The default is gone, so a NOT NULL column must now be supplied.
    let ts = class_oid(&db, Some("public"), "ts");
    assert!(!db.attributes_of(ts)[0].atthasdef);
    build_db(&[(
        "0001.sql",
        "CREATE SEQUENCE s; CREATE TABLE ts (a int DEFAULT nextval('s'));
         ALTER TABLE ts ALTER a DROP DEFAULT;
         DROP SEQUENCE s;",
    )]);
}

#[test]
fn drop_type_reports_a_missing_type_like_pg() {
    // PG 18: 42704 type "nosuch" does not exist.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "DROP TYPE nosuch;")]),
        DdlError::TypeNotFound(_),
        "type \"nosuch\" does not exist",
    );
}

#[test]
fn drop_table_takes_inheritance_children_only_with_cascade() {
    // PG 18: an inheritance child depends on its parent; a partition is
    // part of it; a table's own foreign keys go with it.
    let setup = "CREATE TABLE p (a int);
                 CREATE TABLE c () INHERITS (p);
                 CREATE TABLE c2 () INHERITS (c);";
    let err = try_apply(&[("0001.sql", setup), ("0002.sql", "DROP TABLE p;")]).unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot drop table p because other objects depend on it"),
        "{err}"
    );
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "DROP TABLE p CASCADE;
             CREATE TABLE c (x int);
             CREATE TABLE c2 (x int);
             CREATE TABLE pp (a int) PARTITION BY LIST (a);
             CREATE TABLE pp1 PARTITION OF pp FOR VALUES IN (1);
             DROP TABLE pp;
             CREATE TABLE pp1 (y int);
             CREATE TABLE s (a int PRIMARY KEY, b int REFERENCES s);
             DROP TABLE s;
             CREATE TABLE m1 (a int);
             CREATE TABLE m2 () INHERITS (m1);
             DROP TABLE m1, m2;
             CREATE TABLE m3 (a int);
             CREATE TABLE m4 () INHERITS (m3);
             DROP TABLE m4, m3;",
        ),
    ]);
    assert!(db.resolve_table(None, "m2").is_none());
    assert!(db.resolve_table(None, "m3").is_none());
}
