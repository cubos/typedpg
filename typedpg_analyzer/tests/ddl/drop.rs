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

#[test]
fn drop_type_of_a_range_takes_its_multirange_and_constructors() {
    // The multirange type and the constructor functions are internal to the
    // range type; only what uses them needs CASCADE.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TYPE textrange2 AS RANGE (subtype = text, collation = \"C\");
         DROP TYPE textrange2;
         CREATE TYPE textrange2 AS RANGE (subtype = text);
         CREATE FUNCTION f(textrange2) RETURNS int LANGUAGE sql AS 'SELECT 1';
         DROP TYPE textrange2 CASCADE;
         CREATE TYPE textrange2 AS RANGE (subtype = text);",
    )]);
    assert!(db.resolve_type_by_name(None, "textmultirange2").is_some());
    let err = try_apply(&[(
        "0001.sql",
        "CREATE TYPE textrange2 AS RANGE (subtype = text);
         CREATE TABLE t (a textmultirange2);
         DROP TYPE textrange2;",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot drop type textrange2 because other objects depend on it"),
        "{err}"
    );
}

#[test]
fn drop_table_needs_cascade_for_what_uses_its_row_type() {
    // The row type is internal to the table: a column of another relation
    // holding it (or its array), a domain over it and a view naming it all
    // depend on the table.
    for (setup, dropped) in [
        ("CREATE TABLE a (x int); CREATE TABLE b (c a);", "a"),
        ("CREATE TABLE a (x int); CREATE TABLE b (c a[]);", "a"),
        ("CREATE TABLE a (x int); CREATE TYPE ct AS (y a);", "a"),
        ("CREATE TABLE a (x int); CREATE DOMAIN d AS a;", "a"),
        (
            "CREATE TABLE a (x int); CREATE VIEW v AS SELECT NULL::a AS c;",
            "a",
        ),
    ] {
        let stmt = format!("DROP TABLE {dropped};");
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", &stmt)]).expect_err(setup);
        assert!(
            err.to_string()
                .starts_with("cannot drop table a because other objects depend on it"),
            "{setup}\n  got: {err}"
        );
    }
    // CASCADE drops the columns (not their tables) and the domain; a DROP
    // naming both tables needs none.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE a (x int);
         CREATE TABLE b (c a[], k int);
         CREATE DOMAIN d AS a;
         CREATE TABLE z (c d, k int);
         DROP TABLE a CASCADE;
         CREATE TABLE p (x int);
         CREATE TABLE q (c p);
         DROP TABLE p, q;",
    )]);
    for table in ["b", "z"] {
        let oid = db.resolve_table(None, table).unwrap().oid;
        let names: Vec<&str> = db
            .attributes_of(oid)
            .iter()
            .map(|a| a.attname.as_str())
            .collect();
        assert_eq!(names, ["k"], "{table}");
    }
    assert!(db.resolve_type_by_name(None, "d").is_none());
}

#[test]
fn drop_procedure_signature_may_list_out_arguments() {
    let db = build(&[(
        "0001.sql",
        "CREATE PROCEDURE p(a int, OUT b int) LANGUAGE sql AS 'select a';
         DROP PROCEDURE p(int, int);",
    )]);
    assert!(db.find_functions(None, "p").is_empty());
}

#[test]
fn drop_naming_one_object_twice_drops_it_once() {
    let db = build(&[(
        "0001.sql",
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
         DROP FUNCTION f(int), f(integer);
         CREATE TABLE t (a int);
         CREATE VIEW v AS SELECT * FROM t;
         DROP VIEW v, v;
         CREATE FUNCTION myeq(int, int) RETURNS bool LANGUAGE sql AS 'select true';
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = myeq);
         DROP OPERATOR ===(int, int), ===(int, int);",
    )]);
    assert!(db.find_functions(None, "f").is_empty());
    assert!(db.resolve_table(None, "v").is_none());
}

#[test]
fn drop_errors_for_a_missing_schema_and_several_blocked_targets() {
    assert_ddl_rejections(&[
        (
            "",
            "DROP FUNCTION nosuchschema.f();",
            "schema \"nosuchschema\" does not exist",
        ),
        (
            "CREATE TABLE t (a int);
             CREATE TABLE u (b int);
             CREATE VIEW w AS SELECT * FROM t, u;",
            "DROP TABLE t, u;",
            "cannot drop desired object(s) because other objects depend on them",
        ),
    ]);
    build(&[("0001.sql", "DROP FUNCTION IF EXISTS nosuchschema.f();")]);
}

#[test]
fn objects_built_on_a_function_block_its_drop() {
    let f = "CREATE FUNCTION f() RETURNS int LANGUAGE sql IMMUTABLE AS 'select 1';";
    let blocked = "cannot drop function f() because other objects depend on it";
    let with = |sql: &str| format!("{f} {sql}");
    let cases = [
        with("CREATE TABLE t (a int CHECK (a > f()));"),
        with("CREATE TABLE t (a int); CREATE INDEX ON t ((a + f()));"),
        with("CREATE TABLE t (a int DEFAULT f());"),
        with("CREATE TABLE t (a int, g int GENERATED ALWAYS AS (a + f()) STORED);"),
        with("CREATE DOMAIN d AS int CHECK (VALUE > f());"),
        with("CREATE FUNCTION g(x int DEFAULT f()) RETURNS int LANGUAGE sql AS 'select 1';"),
        with("CREATE TABLE t (a int); CREATE POLICY p ON t USING (a = f());"),
        with("CREATE FUNCTION g() RETURNS int LANGUAGE sql RETURN f() + 1;"),
    ];
    let cases: Vec<(&str, &str, &str)> = cases
        .iter()
        .map(|setup| (setup.as_str(), "DROP FUNCTION f();", blocked))
        .collect();
    assert_ddl_rejections(&cases);
    assert_ddl_rejections(&[
        (
            "CREATE FUNCTION myf(int, int) RETURNS int LANGUAGE sql AS 'select $1';
             CREATE OPERATOR === (leftarg = int, rightarg = int, function = myf);",
            "DROP FUNCTION myf(int, int);",
            "cannot drop function myf(integer,integer) because other objects depend on it",
        ),
        (
            "CREATE TYPE mood AS ENUM ('a');
             CREATE FUNCTION m2t(mood) RETURNS text LANGUAGE sql IMMUTABLE AS 'select $1::text';
             CREATE CAST (mood AS text) WITH FUNCTION m2t(mood);",
            "DROP FUNCTION m2t(mood);",
            "cannot drop function m2t(mood) because other objects depend on it",
        ),
        (
            "CREATE FUNCTION sf(int, int) RETURNS int LANGUAGE sql AS 'select $1 + $2';
             CREATE AGGREGATE ag(int) (sfunc = sf, stype = int);",
            "DROP FUNCTION sf(int, int);",
            "cannot drop function sf(integer,integer) because other objects depend on it",
        ),
        (
            "CREATE TYPE base2;
             CREATE FUNCTION base2_in(cstring) RETURNS base2 LANGUAGE internal IMMUTABLE STRICT
                 AS 'int4in';
             CREATE FUNCTION base2_out(base2) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT
                 AS 'int4out';
             CREATE TYPE base2 (INPUT = base2_in, OUTPUT = base2_out, INTERNALLENGTH = 4,
                 PASSEDBYVALUE);",
            "DROP FUNCTION base2_in(cstring);",
            "cannot drop function base2_in(cstring) because other objects depend on it",
        ),
    ]);
}

#[test]
fn drop_function_cascade_takes_what_is_built_on_it() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION myf(int, int) RETURNS int LANGUAGE sql AS 'select $1';
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = myf);
         CREATE FUNCTION f() RETURNS int LANGUAGE sql IMMUTABLE AS 'select 1';
         CREATE TABLE tg (a int, g int GENERATED ALWAYS AS (a + f()) STORED);
         CREATE FUNCTION g(x int DEFAULT f()) RETURNS int LANGUAGE sql AS 'select 1';",
    )
    .unwrap();
    db.apply_sql("DROP FUNCTION myf(int, int) CASCADE; DROP FUNCTION f() CASCADE;")
        .unwrap();
    assert_err_prefix!(
        db.analyze("SELECT 1 === 2 AS x"),
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: integer === integer"
    );
    assert!(db.find_functions(None, "g").is_empty());
    // The generated column went with f(); the table stays.
    let q = db.analyze("SELECT * FROM tg").unwrap();
    assert_eq!(q.columns.len(), 1);
}

#[test]
fn an_inline_sql_body_depends_on_what_it_reads() {
    let setup = "CREATE TABLE t (a int);
                 CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a FROM t; END;";
    assert_ddl_rejections(&[
        (
            setup,
            "DROP TABLE t;",
            "cannot drop table t because other objects depend on it",
        ),
        (
            setup,
            "ALTER TABLE t DROP COLUMN a;",
            "cannot drop column a of table t because other objects depend on it",
        ),
    ]);
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(setup).unwrap();
    db.apply_sql("DROP TABLE t CASCADE;").unwrap();
    assert_err_prefix!(
        db.analyze("SELECT f() AS x"),
        AnalyzeError::UndefinedFunction(_),
        "function f() does not exist"
    );
    // A string body is not parsed into dependencies.
    build(&[(
        "0001.sql",
        "CREATE TABLE t (a int);
         CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT a FROM t';
         DROP TABLE t;",
    )]);
}

#[test]
fn rules_policies_triggers_and_views_hold_on_to_what_they_use() {
    assert_ddl_rejections(&[
        (
            "CREATE SEQUENCE s;
             CREATE VIEW vs AS SELECT nextval('s') AS n;",
            "DROP SEQUENCE s;",
            "cannot drop sequence s because other objects depend on it",
        ),
        (
            "CREATE TYPE comp AS (a int, b int);
             CREATE VIEW vc AS SELECT (NULL::comp).b;",
            "ALTER TYPE comp DROP ATTRIBUTE b;",
            "cannot drop column b of composite type comp because other objects depend on it",
        ),
        (
            "CREATE TABLE ta (a int, b int);
             CREATE TABLE tr (a int);
             CREATE RULE r AS ON INSERT TO tr DO ALSO INSERT INTO ta VALUES (NEW.a);",
            "DROP TABLE ta;",
            "cannot drop table ta because other objects depend on it",
        ),
        (
            "CREATE TABLE ta (a int, b int);
             CREATE TABLE tr (a int);
             CREATE RULE r AS ON INSERT TO tr DO ALSO INSERT INTO ta VALUES (NEW.a);",
            "ALTER TABLE ta DROP COLUMN a;",
            "cannot drop column a of table ta because other objects depend on it",
        ),
        (
            "CREATE TABLE ta (a int, b int);
             CREATE TABLE tp (a int);
             CREATE POLICY p ON tp USING (a IN (SELECT b FROM ta));",
            "ALTER TABLE ta DROP COLUMN b;",
            "cannot drop column b of table ta because other objects depend on it",
        ),
        (
            "CREATE TABLE ta (a int, b int);
             CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
             CREATE TRIGGER tt BEFORE UPDATE OF b ON ta FOR EACH ROW EXECUTE FUNCTION trf();",
            "ALTER TABLE ta DROP COLUMN b;",
            "cannot drop column b of table ta because other objects depend on it",
        ),
        (
            "CREATE TABLE tid (a int GENERATED ALWAYS AS IDENTITY);",
            "DROP SEQUENCE tid_a_seq;",
            "cannot drop sequence tid_a_seq because column a of table tid requires it",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY mydict (TEMPLATE = simple);
             CREATE TEXT SEARCH CONFIGURATION mycfg (COPY = simple);
             ALTER TEXT SEARCH CONFIGURATION mycfg ALTER MAPPING FOR word WITH mydict;",
            "DROP TEXT SEARCH DICTIONARY mydict;",
            "cannot drop text search dictionary mydict because other objects depend on it",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION mycfg (COPY = simple);
             CREATE TABLE tts (d text);
             CREATE INDEX ON tts (to_tsvector('mycfg', d));",
            "DROP TEXT SEARCH CONFIGURATION mycfg;",
            "cannot drop text search configuration mycfg because other objects depend on it",
        ),
    ]);
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE SEQUENCE s;
         CREATE VIEW vs AS SELECT nextval('s') AS n;
         DROP SEQUENCE s CASCADE;",
    )
    .unwrap();
    assert!(db.resolve_table(None, "vs").is_none());
}

#[test]
fn a_publication_holds_on_to_its_column_list_and_row_filter() {
    let setup = "CREATE TABLE tp (a int, b int, c int);
                 CREATE PUBLICATION pb FOR TABLE tp (a, b);
                 CREATE TABLE tq (a int, b int);
                 CREATE PUBLICATION pq FOR TABLE tq WHERE (b > 0);";
    assert_ddl_rejections(&[
        (
            setup,
            "ALTER TABLE tp DROP COLUMN b;",
            "cannot drop column b of table tp because other objects depend on it",
        ),
        (
            setup,
            "ALTER TABLE tq DROP COLUMN b;",
            "cannot drop column b of table tq because other objects depend on it",
        ),
    ]);
    build(&[("0001.sql", &format!("{setup} ALTER TABLE tp DROP COLUMN c;"))]);
}
