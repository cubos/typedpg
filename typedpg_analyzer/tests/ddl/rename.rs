//! RENAME TABLE / COLUMN / SCHEMA and their downstream effects: dependent
//! views get their AST rewritten, function references get re-bound.

use crate::common::*;

// ── RENAME TABLE and CASCADE detection ──────────────────────────────────────

#[test]
fn rename_table_preserves_cascade_detection() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME TO t2;"),
        ("0003.sql", "DROP TABLE t2 CASCADE;"),
    ]);

    assert!(snap.resolve_table(None, "t2").is_none());
    assert!(
        snap.resolve_table(None, "v").is_none(),
        "view should have been dropped via CASCADE through the renamed dep",
    );
}

#[test]
fn rename_table_without_cascade_still_blocks_drop() {
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME TO t2;"),
        ("0003.sql", "DROP TABLE t2;"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cannot drop table t2 because other objects depend on it (view(s) public.v depend on this)"
    );
}

// ── AST rewriting on RENAME propagates into dependent view definitions ─────

#[test]
fn rename_table_rewrites_view_ast() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL);
             CREATE VIEW v AS SELECT id FROM t;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME TO nodes;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    assert!(snap.view_body(view.oid).is_some());

    let table_deps = view_table_deps(&snap, view.oid);
    assert!(
        !table_deps
            .iter()
            .any(|k| k.name == "t" && k.schema == "public")
    );
    assert!(
        table_deps
            .iter()
            .any(|k| k.name == "nodes" && k.schema == "public")
    );
}

#[test]
fn rename_column_rewrites_deps_with_self_join() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, parent_id INT);
             CREATE VIEW v AS
                 SELECT a.id, b.id AS parent_id
                 FROM t a JOIN t b ON b.id = a.parent_id;",
        ),
        ("0002.sql", "ALTER TABLE t RENAME COLUMN id TO node_id;"),
    ]);

    let view = snap.resolve_table(None, "v").unwrap();
    let col_deps = view_column_deps(&snap, view.oid);
    let t = QualifiedName::new("public", "t");
    assert!(col_deps.iter().any(|(k, c)| k == &t && c == "node_id"));
    assert!(col_deps.iter().all(|(_, c)| c != "id"));
}

#[test]
fn rename_schema_rewrites_view_ast() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE SCHEMA app;
             CREATE TABLE app.t (id INT NOT NULL);
             CREATE VIEW app.v AS SELECT id FROM app.t;",
        ),
        ("0002.sql", "ALTER SCHEMA app RENAME TO core;"),
    ]);

    let view = snap.resolve_table(Some("core"), "v").unwrap();
    let table_deps = view_table_deps(&snap, view.oid);
    let new = QualifiedName::new("core", "t");
    assert!(table_deps.contains(&new));
    assert!(!table_deps.iter().any(|k| k.schema == "app"));
}

#[test]
fn renaming_an_inherited_column_follows_renameatt_internal() {
    // PG 18: ONLY is refused while a child exists — before the column is
    // even looked up — and a column a child also inherits from outside the
    // renamed tree can't be renamed.
    let setup = "CREATE TABLE inht1 (a int, b int);
                 CREATE TABLE inht2 (x int) INHERITS (inht1);
                 CREATE TABLE inht3 (y int) INHERITS (inht1);
                 CREATE TABLE inht4 (z int) INHERITS (inht2, inht3);
                 CREATE TABLE other (x int);
                 CREATE TABLE both_ (q int) INHERITS (inht2, other);";
    for (stmt, msg) in [
        (
            "ALTER TABLE ONLY inht1 RENAME nosuch TO c;",
            "inherited column \"nosuch\" must be renamed in child tables too",
        ),
        (
            "ALTER TABLE inht2 RENAME a TO aa;",
            "cannot rename inherited column \"a\"",
        ),
        (
            "ALTER TABLE inht2 RENAME x TO xx;",
            "cannot rename inherited column \"x\"",
        ),
        (
            "ALTER TABLE inht1 RENAME xmin TO c;",
            "cannot rename system column \"xmin\"",
        ),
        (
            "ALTER TABLE inht1 RENAME a TO ctid;",
            "column name \"ctid\" conflicts with a system column name",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE inht1 RENAME a TO aa; ALTER TABLE inht3 RENAME y TO yy;",
        ),
    ]);
    let t4 = db.resolve_table(None, "inht4").unwrap();
    assert!(db.attributes_of(t4.oid).iter().any(|a| a.attname == "aa"));
}

#[test]
fn rename_checks_the_relation_kind() {
    // RangeVarCallbackForAlterRelation: ALTER VIEW / SEQUENCE name one,
    // ALTER TABLE doesn't reach a composite type; ALTER INDEX / TABLE may
    // rename either.
    let setup = "CREATE TABLE b (x int);
                 CREATE INDEX bi ON b (x);
                 CREATE TYPE ct AS (x int);";
    for (stmt, msg) in [
        ("ALTER TABLE ct RENAME TO ct2;", "\"ct\" is a composite type"),
        ("ALTER VIEW b RENAME TO c;", "\"b\" is not a view"),
        ("ALTER SEQUENCE b RENAME TO c;", "\"b\" is not a sequence"),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE bi RENAME TO bj; ALTER INDEX b RENAME TO c;",
        ),
    ]);
}
