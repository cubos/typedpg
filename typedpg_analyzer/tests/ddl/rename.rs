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
        (
            "ALTER TABLE ct RENAME TO ct2;",
            "\"ct\" is a composite type",
        ),
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

#[test]
fn dependencies_survive_renames() {
    // PG keeps pg_depend rows by OID, so renaming either side of a
    // dependency — the dependent (a policy, trigger, rule, domain
    // constraint, publication, text search object) or what it refers to
    // (a table, column, function, language) — keeps it: the DROP is still
    // refused, and CASCADE still takes the dependent along.
    for (setup, drop, refused, cascade, gone, gone_msg) in [
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql IMMUTABLE AS 'select 1';
             CREATE TABLE tq (a int);
             CREATE POLICY p ON tq USING (a = f());
             ALTER POLICY p ON tq RENAME TO p2;
             ALTER TABLE tq RENAME TO tq2;",
            "DROP FUNCTION f();",
            "cannot drop function f() because other objects depend on it",
            "DROP FUNCTION f() CASCADE;",
            "ALTER POLICY p2 ON tq2 RENAME TO p3;",
            "policy \"p2\" for table \"tq2\" does not exist",
        ),
        (
            "CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql
                 AS 'begin return new; end';
             CREATE TABLE tt (a int, b int);
             CREATE TRIGGER t1 BEFORE UPDATE OF b ON tt
                 FOR EACH ROW EXECUTE FUNCTION trf();
             ALTER TRIGGER t1 ON tt RENAME TO t2;
             ALTER TABLE tt RENAME COLUMN b TO b2;
             ALTER TABLE tt RENAME TO tt2;",
            "ALTER TABLE tt2 DROP COLUMN b2;",
            "cannot drop column b2 of table tt2 because other objects depend on it \
             (trigger t2 on table tt2 depends on column b2 of table tt2)",
            "ALTER TABLE tt2 DROP COLUMN b2 CASCADE;",
            "DROP TRIGGER t2 ON tt2;",
            "trigger \"t2\" for table \"tt2\" does not exist",
        ),
        (
            "CREATE TABLE tr (a int);
             CREATE TABLE ta (a int, b int);
             CREATE RULE r AS ON INSERT TO tr DO ALSO INSERT INTO ta VALUES (NEW.a);
             ALTER RULE r ON tr RENAME TO r2;
             ALTER TABLE ta RENAME TO ta2;
             ALTER TABLE ta2 RENAME COLUMN a TO a2;",
            "ALTER TABLE ta2 DROP COLUMN a2;",
            "cannot drop column a2 of table ta2 because other objects depend on it \
             (rule r2 on table tr depends on column a2 of table ta2)",
            "ALTER TABLE ta2 DROP COLUMN a2 CASCADE;",
            "DROP RULE r2 ON tr;",
            "rule \"r2\" for relation \"tr\" does not exist",
        ),
        (
            "CREATE FUNCTION h() RETURNS int LANGUAGE sql IMMUTABLE AS 'select 1';
             CREATE DOMAIN dm AS int CONSTRAINT c1 CHECK (VALUE > h());
             ALTER DOMAIN dm RENAME CONSTRAINT c1 TO c2;
             ALTER FUNCTION h() RENAME TO h2;",
            "DROP FUNCTION h2();",
            "cannot drop function h2() because other objects depend on it \
             (constraint c2 depends on function h2())",
            "DROP FUNCTION h2() CASCADE;",
            "ALTER DOMAIN dm DROP CONSTRAINT c2;",
            "constraint \"c2\" of domain \"dm\" does not exist",
        ),
        (
            "CREATE TABLE tp (a int, b int, c int);
             CREATE PUBLICATION pb FOR TABLE tp (a, b);
             ALTER PUBLICATION pb RENAME TO pb2;
             ALTER TABLE tp RENAME COLUMN b TO bb;",
            "ALTER TABLE tp DROP COLUMN bb;",
            "cannot drop column bb of table tp because other objects depend on it \
             (publication of table tp in publication pb2 depends on column bb of table tp)",
            "ALTER TABLE tp DROP COLUMN bb CASCADE;",
            "ALTER PUBLICATION pb2 DROP TABLE tp;",
            "relation \"tp\" is not part of the publication",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY mydict (TEMPLATE = simple);
             CREATE TEXT SEARCH CONFIGURATION mycfg (COPY = simple);
             ALTER TEXT SEARCH CONFIGURATION mycfg ALTER MAPPING FOR word WITH mydict;
             ALTER TEXT SEARCH DICTIONARY mydict RENAME TO mydict2;
             ALTER TEXT SEARCH CONFIGURATION mycfg RENAME TO mycfg2;",
            "DROP TEXT SEARCH DICTIONARY mydict2;",
            "cannot drop text search dictionary mydict2 because other objects depend on it \
             (text search configuration mycfg2 depends on text search dictionary mydict2)",
            "DROP TEXT SEARCH DICTIONARY mydict2 CASCADE;",
            "DROP TEXT SEARCH CONFIGURATION mycfg2;",
            "text search configuration \"mycfg2\" does not exist",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION cfg (COPY = simple);
             CREATE TABLE tts (d text);
             CREATE INDEX tts_idx ON tts (to_tsvector('cfg', d));
             ALTER TEXT SEARCH CONFIGURATION cfg RENAME TO cfg2;",
            "DROP TEXT SEARCH CONFIGURATION cfg2;",
            "cannot drop text search configuration cfg2 because other objects depend on it \
             (index tts_idx depends on text search configuration cfg2)",
            "DROP TEXT SEARCH CONFIGURATION cfg2 CASCADE;",
            "DROP INDEX tts_idx;",
            "index \"tts_idx\" does not exist",
        ),
        (
            "CREATE FUNCTION pf() RETURNS int LANGUAGE plpgsql AS 'begin return 1; end';
             ALTER LANGUAGE plpgsql RENAME TO plx;",
            "DROP EXTENSION plpgsql;",
            "cannot drop extension plpgsql because other objects depend on it \
             (function pf() depends on language plx)",
            "DROP EXTENSION plpgsql CASCADE;",
            "DROP FUNCTION pf();",
            "function pf() does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", drop)]).expect_err(drop);
        assert!(err.to_string().starts_with(refused), "{drop}\n  got: {err}");
        let err = try_apply(&[
            ("0001.sql", setup),
            ("0002.sql", cascade),
            ("0003.sql", gone),
        ])
        .expect_err(gone);
        assert!(
            err.to_string().starts_with(gone_msg),
            "{gone}\n  got: {err}"
        );
    }
}
