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

#[test]
fn create_table_as_does_not_depend_on_its_source() {
    // CREATE TABLE AS stores rows, not the query: the new table depends on
    // its columns' types only (intorel_startup → heap_create_with_catalog).
    for setup in [
        "CREATE TABLE src (a int);
         CREATE TABLE dst AS SELECT a FROM src;",
        "CREATE TABLE src (i int, j int);
         CREATE TABLE dst (ii, jj) AS SELECT i, j FROM src WITH NO DATA;",
        "CREATE TABLE src (a int);
         SELECT a INTO dst FROM src;",
        "CREATE TABLE src (a int);
         CREATE VIEW vsrc AS SELECT a FROM src;
         CREATE TABLE dst AS SELECT a FROM vsrc;
         DROP VIEW vsrc;",
    ] {
        build(&[("0001.sql", setup), ("0002.sql", "DROP TABLE src;")]);
    }
    build(&[(
        "0001.sql",
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'select 1';
         CREATE TABLE dst AS SELECT f() AS x;
         DROP FUNCTION f();",
    )]);
    assert_ddl_rejections(&[
        // A materialized view keeps its query, and what it reads.
        (
            "CREATE TABLE src (a int);
             CREATE MATERIALIZED VIEW mv AS SELECT a FROM src;",
            "DROP TABLE src;",
            "cannot drop table src because other objects depend on it",
        ),
        // The new table's columns hold on to their types.
        (
            "CREATE TYPE e AS ENUM ('x');
             CREATE TABLE src (a e);
             CREATE TABLE dst AS SELECT a FROM src;
             DROP TABLE src;",
            "DROP TYPE e;",
            "cannot drop type e because other objects depend on it",
        ),
    ]);
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

#[test]
fn dropping_a_composite_type_releases_its_attribute_types() {
    build(&[(
        "0001.sql",
        "CREATE DOMAIN js_nn AS int NOT NULL;
         CREATE TYPE rec_nn AS (i js_nn);
         DROP TYPE rec_nn;
         DROP DOMAIN js_nn;",
    )]);
    build(&[(
        "0001.sql",
        "CREATE TYPE e AS ENUM ('x');
         CREATE TYPE r AS (a int, b e);
         ALTER TYPE r DROP ATTRIBUTE b;
         DROP TYPE e;",
    )]);
    // While the composite type stands, its attributes pin their types.
    assert_ddl_rejections(&[(
        "CREATE DOMAIN js_nn AS int NOT NULL;
         CREATE TYPE rec_nn AS (i js_nn);",
        "DROP DOMAIN js_nn;",
        "cannot drop type js_nn because other objects depend on it",
    )]);
}

#[test]
fn a_domain_over_a_domain_pins_its_base() {
    assert_ddl_rejections(&[
        (
            "CREATE DOMAIN ddt int4;
             CREATE DOMAIN dependent ddt;",
            "DROP DOMAIN ddt;",
            "cannot drop type ddt because other objects depend on it",
        ),
        (
            "CREATE TYPE e AS ENUM ('x');
             CREATE DOMAIN de e;",
            "DROP TYPE e;",
            "cannot drop type e because other objects depend on it",
        ),
    ]);
    // CASCADE takes the domains built on it, and what uses them.
    let db = build_db(&[(
        "0001.sql",
        "CREATE DOMAIN ddt int4;
         CREATE DOMAIN dependent ddt;
         CREATE DOMAIN dependent2 dependent;
         CREATE TABLE td (a int, d dependent2);
         DROP DOMAIN ddt CASCADE;",
    )]);
    assert!(db.resolve_type_by_name(None, "dependent").is_none());
    assert!(db.resolve_type_by_name(None, "dependent2").is_none());
    let q = db.analyze("SELECT * FROM td").unwrap();
    assert_eq!(q.columns.len(), 1);
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
fn a_partition_key_expression_pins_its_function() {
    assert_ddl_rejections(&[
        (
            "CREATE FUNCTION plusone(a int) RETURNS int AS $$ SELECT a+1; $$ LANGUAGE sql IMMUTABLE;
             CREATE TABLE pk (a int, b int) PARTITION BY RANGE (plusone(b));",
            "DROP FUNCTION plusone(int);",
            "cannot drop function plusone(integer) because other objects depend on it",
        ),
        (
            "CREATE FUNCTION plusone(a int) RETURNS int AS $$ SELECT a+1; $$ LANGUAGE sql IMMUTABLE;
             CREATE TABLE pk (a int, b int) PARTITION BY RANGE (a, (plusone(b) * 2));",
            "DROP FUNCTION plusone(int);",
            "cannot drop function plusone(integer) because other objects depend on it",
        ),
    ]);
    // The key is part of the table: CASCADE drops the table.
    let db = build_db(&[(
        "0001.sql",
        "CREATE FUNCTION plusone(a int) RETURNS int AS $$ SELECT a+1; $$ LANGUAGE sql IMMUTABLE;
         CREATE TABLE pk (a int, b int) PARTITION BY RANGE (plusone(b));
         CREATE TABLE pk1 PARTITION OF pk FOR VALUES FROM (1) TO (10);
         DROP FUNCTION plusone(int) CASCADE;",
    )]);
    assert!(db.resolve_table(None, "pk").is_none());
    assert!(db.resolve_table(None, "pk1").is_none());
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
fn a_functionally_dependent_view_pins_the_primary_key() {
    // A view selecting columns grouped through the primary key relies on
    // the key (check_functional_grouping → query->constraintDeps).
    let setup = "CREATE TABLE articles (id int CONSTRAINT articles_pkey PRIMARY KEY, title text);
                 CREATE VIEW av AS SELECT id, title FROM articles GROUP BY id;";
    assert_ddl_rejections(&[
        (
            setup,
            "ALTER TABLE articles DROP CONSTRAINT articles_pkey RESTRICT;",
            "cannot drop constraint articles_pkey on table articles because other objects depend on it",
        ),
        (
            "CREATE TABLE articles (id int CONSTRAINT articles_pkey PRIMARY KEY, title text);
             CREATE FUNCTION af() RETURNS SETOF text LANGUAGE sql
                 BEGIN ATOMIC SELECT title FROM articles GROUP BY id; END;",
            "ALTER TABLE articles DROP CONSTRAINT articles_pkey;",
            "cannot drop constraint articles_pkey on table articles because other objects depend on it",
        ),
    ]);
    let db = build_db(&[(
        "0001.sql",
        &format!("{setup} ALTER TABLE articles DROP CONSTRAINT articles_pkey CASCADE;"),
    )]);
    assert!(db.resolve_table(None, "av").is_none());
    // Grouping by every column it selects, the view needs no key.
    build(&[(
        "0001.sql",
        "CREATE TABLE articles (id int CONSTRAINT articles_pkey PRIMARY KEY, title text);
         CREATE VIEW av AS SELECT id, title FROM articles GROUP BY id, title;
         CREATE VIEW av2 AS SELECT id FROM articles GROUP BY id;
         ALTER TABLE articles DROP CONSTRAINT articles_pkey;",
    )]);
}

#[test]
fn a_view_over_a_rowtype_function_pins_the_table_columns() {
    // regress/sql/create_view.sql (tt14t): the view's columns come from
    // tt14t's row type, through the function (process_function_rte_ref).
    let base = "CREATE TABLE tt14t (f1 text, f3 text, f4 text);
                CREATE FUNCTION tt14f() RETURNS SETOF tt14t AS 'select * from tt14t' LANGUAGE sql;";
    assert_ddl_rejections(&[
        (
            &format!("{base} CREATE VIEW tt14v AS SELECT t.* FROM tt14f() t;"),
            "ALTER TABLE tt14t DROP COLUMN f3;",
            "cannot drop column f3 of table tt14t because other objects depend on it",
        ),
        (
            &format!("{base} CREATE VIEW tt14v AS SELECT t.f1, t.f4 FROM tt14f() t;"),
            "ALTER TABLE tt14t ALTER COLUMN f4 TYPE integer USING f4::integer;",
            "cannot alter type of a column used by a view or rule",
        ),
        (
            &format!(
                "{base} ALTER TABLE tt14t DROP COLUMN f1;
                 CREATE VIEW tt14v AS SELECT f4 FROM tt14f() WITH ORDINALITY AS t;"
            ),
            "ALTER TABLE tt14t DROP COLUMN f4;",
            "cannot drop column f4 of table tt14t because other objects depend on it",
        ),
        // A composite type's attribute, read under a column alias.
        (
            "CREATE TYPE ct AS (x int, y int);
             CREATE FUNCTION cf() RETURNS ct LANGUAGE sql AS 'select 1, 2';
             CREATE VIEW cv AS SELECT b FROM cf() AS f(a, b);
             ALTER TYPE ct DROP ATTRIBUTE x;",
            "ALTER TYPE ct DROP ATTRIBUTE y;",
            "cannot drop column y of composite type ct because other objects depend on it",
        ),
    ]);
    // Only the columns the view reads are pinned; CASCADE takes the view.
    let db = build_db(&[(
        "0001.sql",
        &format!(
            "{base} CREATE VIEW tt14v AS SELECT t.f1, t.f4 FROM tt14f() t;
             ALTER TABLE tt14t DROP COLUMN f3;
             ALTER TABLE tt14t DROP COLUMN f4 CASCADE;"
        ),
    )]);
    assert!(db.resolve_table(None, "tt14v").is_none());
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
    build(&[(
        "0001.sql",
        &format!("{setup} ALTER TABLE tp DROP COLUMN c;"),
    )]);
}

#[test]
fn dependency_errors_qualify_names_off_the_search_path() {
    // getObjectDescription qualifies a relation, routine, type or text
    // search object that isn't visible — as after SET SCHEMA into a schema
    // off the search path — and quotes its name when needed.
    let setup = "CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql
                     AS 'begin return new; end';
                 CREATE SCHEMA s2;
                 CREATE TABLE tc (a int, b int);
                 CREATE TRIGGER tc1 BEFORE UPDATE OF b ON tc
                     FOR EACH ROW EXECUTE FUNCTION trf();
                 ALTER TABLE tc SET SCHEMA s2;
                 CREATE FUNCTION k() RETURNS int LANGUAGE sql IMMUTABLE AS 'select 1';
                 CREATE TABLE tk (a int);
                 CREATE POLICY pk ON tk USING (a = k());
                 ALTER FUNCTION k() SET SCHEMA s2;
                 CREATE TABLE x (a int);
                 CREATE VIEW vx AS SELECT a FROM x;
                 ALTER TABLE x SET SCHEMA s2;
                 CREATE DOMAIN dd AS int CHECK (VALUE > 0);
                 CREATE TABLE ud (v dd);
                 ALTER DOMAIN dd SET SCHEMA s2;
                 CREATE TEXT SEARCH DICTIONARY mydict (TEMPLATE = simple);
                 CREATE TEXT SEARCH CONFIGURATION mycfg (COPY = simple);
                 ALTER TEXT SEARCH CONFIGURATION mycfg ALTER MAPPING FOR word WITH mydict;
                 ALTER TEXT SEARCH DICTIONARY mydict SET SCHEMA s2;
                 ALTER TEXT SEARCH CONFIGURATION mycfg SET SCHEMA s2;
                 CREATE TABLE \"My T\" (a int);
                 CREATE VIEW vm AS SELECT a FROM \"My T\";
                 CREATE FUNCTION \"Fx\"() RETURNS int LANGUAGE sql IMMUTABLE AS 'select 1';
                 CREATE POLICY pf ON tk USING (a = \"Fx\"());
                 CREATE TYPE \"Ty\" AS ENUM ('a');
                 CREATE TABLE ut (x \"Ty\");";
    for (stmt, msg) in [
        (
            "ALTER TABLE s2.tc DROP COLUMN b;",
            "cannot drop column b of table s2.tc because other objects depend on it \
             (trigger tc1 on table s2.tc depends on column b of table s2.tc)",
        ),
        (
            "DROP FUNCTION s2.k();",
            "cannot drop function s2.k() because other objects depend on it \
             (policy pk on table tk depends on function s2.k())",
        ),
        (
            "ALTER TABLE s2.x DROP COLUMN a;",
            "cannot drop column a of table s2.x because other objects depend on it",
        ),
        (
            "DROP TABLE s2.x;",
            "cannot drop table s2.x because other objects depend on it",
        ),
        (
            "DROP DOMAIN s2.dd;",
            "cannot drop type s2.dd because other objects depend on it",
        ),
        (
            "DROP TEXT SEARCH DICTIONARY s2.mydict;",
            "cannot drop text search dictionary s2.mydict because other objects depend on it \
             (text search configuration s2.mycfg depends on text search dictionary s2.mydict)",
        ),
        (
            "DROP TABLE \"My T\";",
            "cannot drop table \"My T\" because other objects depend on it",
        ),
        (
            "DROP FUNCTION \"Fx\"();",
            "cannot drop function \"Fx\"() because other objects depend on it \
             (policy pf on table tk depends on function \"Fx\"())",
        ),
        (
            "DROP TYPE \"Ty\";",
            "cannot drop type \"Ty\" because other objects depend on it",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
}

#[test]
fn a_partition_dependent_blocks_the_drop_of_its_parent() {
    // reportDependentObjects names what the DROP names, not the partition
    // that goes along with it.
    let setup = "CREATE TABLE p (a int) PARTITION BY RANGE (a);
                 CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (1) TO (10)
                     PARTITION BY RANGE (a);
                 CREATE TABLE p11 PARTITION OF p1 FOR VALUES FROM (1) TO (5);
                 CREATE VIEW vp AS SELECT a FROM p11;
                 CREATE TABLE q (a int) PARTITION BY LIST (a);
                 CREATE TABLE q1 PARTITION OF q FOR VALUES IN (1);
                 CREATE FUNCTION qf() RETURNS SETOF q1 LANGUAGE sql
                     BEGIN ATOMIC SELECT * FROM q1; END;";
    for (stmt, msg) in [
        (
            "DROP TABLE p;",
            "cannot drop table p because other objects depend on it",
        ),
        (
            "DROP TABLE p1;",
            "cannot drop table p1 because other objects depend on it",
        ),
        (
            "DROP TABLE q;",
            "cannot drop table q because other objects depend on it",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        ("0002.sql", "DROP TABLE p CASCADE; DROP TABLE q CASCADE;"),
    ]);
}

// ── Objects that are part of others ─────────────────────────────────────────

#[test]
fn a_range_types_internal_objects_go_only_with_it() {
    // DefineRange: the multirange type, the constructors of both and the
    // range → multirange cast (through `mr(range)`) are part of the range
    // type — DEPENDENCY_INTERNAL, which CASCADE doesn't override.
    let range = "CREATE TYPE textrange AS RANGE (subtype = text);";
    assert_ddl_rejections(&[
        (
            range,
            "DROP CAST (textrange AS textmultirange);",
            "cannot drop cast from textrange to textmultirange because function \
             textmultirange(textrange) requires it",
        ),
        (
            range,
            "DROP CAST (textrange AS textmultirange) CASCADE;",
            "cannot drop cast from textrange to textmultirange because function \
             textmultirange(textrange) requires it",
        ),
        (
            range,
            "DROP TYPE textmultirange;",
            "cannot drop type textmultirange because type textrange requires it",
        ),
        (
            range,
            "DROP FUNCTION textmultirange(textrange) CASCADE;",
            "cannot drop function textmultirange(textrange) because type textmultirange \
             requires it",
        ),
        (
            range,
            "DROP FUNCTION textrange(text, text);",
            "cannot drop function textrange(text,text) because type textrange requires it",
        ),
        (
            range,
            "DROP TYPE textmultirange[];",
            "cannot drop type textmultirange[] because type textmultirange requires it",
        ),
        (
            range,
            "CREATE CAST (textrange AS textmultirange) WITH INOUT;",
            "cast from type textrange to type textmultirange already exists",
        ),
        // The multirange constructors live in the range type's schema,
        // whatever the multirange type's.
        (
            "CREATE SCHEMA s;
             CREATE TYPE textrange AS RANGE (subtype = text, multirange_type_name = s.tmr);",
            "DROP CAST (textrange AS s.tmr);",
            "cannot drop cast from textrange to s.tmr because function tmr(textrange) \
             requires it",
        ),
        // A view casting to the multirange calls the constructor.
        (
            "CREATE TYPE textrange AS RANGE (subtype = text);
             CREATE VIEW v AS SELECT textrange('a', 'b')::textmultirange AS m;",
            "DROP TYPE textrange;",
            "cannot drop type textrange because other objects depend on it",
        ),
    ]);
    // The range type takes them all along; naming them in the same DROP is
    // fine.
    build(&[(
        "0001.sql",
        "CREATE TYPE textrange AS RANGE (subtype = text);
         CREATE VIEW v AS SELECT textrange('a', 'b')::textmultirange AS m;
         DROP TYPE textrange CASCADE;
         CREATE TYPE textmultirange AS ENUM ('x');
         CREATE FUNCTION textmultirange(int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE TYPE r1 AS RANGE (subtype = text);
         DROP TYPE r1, r1_multirange;
         CREATE TYPE r2 AS RANGE (subtype = text);
         DROP TYPE r2_multirange, r2;
         CREATE TYPE r3 AS RANGE (subtype = text);
         DROP TYPE r3;
         CREATE TYPE r3 AS RANGE (subtype = text);",
    )]);
}

#[test]
fn array_and_row_types_go_only_with_their_owner() {
    // GenerateTypeDependencies: an implicit array type is part of its
    // element type, a relation's row type part of the relation.
    let setup = "CREATE TYPE e AS ENUM ('x');
                 CREATE TYPE c AS (a int);
                 CREATE DOMAIN d AS int;
                 CREATE TABLE t (a int);
                 CREATE VIEW v AS SELECT 1 AS a;";
    assert_ddl_rejections(&[
        (
            setup,
            "DROP TYPE e[];",
            "cannot drop type e[] because type e requires it",
        ),
        (
            setup,
            "DROP TYPE _e CASCADE;",
            "cannot drop type e[] because type e requires it",
        ),
        (
            setup,
            "DROP TYPE c[];",
            "cannot drop type c[] because type c requires it",
        ),
        (
            setup,
            "DROP TYPE d[];",
            "cannot drop type d[] because type d requires it",
        ),
        (setup, "DROP DOMAIN d[];", "\"d[]\" is not a domain"),
        (
            setup,
            "DROP TYPE t;",
            "cannot drop type t because table t requires it",
        ),
        (
            setup,
            "DROP TYPE _t;",
            "cannot drop type t[] because type t requires it",
        ),
        (
            setup,
            "DROP TYPE v;",
            "cannot drop type v because view v requires it",
        ),
        (
            setup,
            "DROP TYPE v[];",
            "cannot drop type v[] because type v requires it",
        ),
        (
            "",
            "DROP TYPE nonexist[];",
            "type \"nonexist[]\" does not exist",
        ),
    ]);
    // `e[]` names the array type, not e: e stays.
    let db = build(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "DROP TYPE IF EXISTS nonexist[];
             CREATE TYPE e2 AS ENUM ('y');
             DROP TYPE e2[], e2;
             CREATE TYPE e3 AS ENUM ('y');
             DROP TYPE e3, e3[];",
        ),
    ]);
    assert!(db.resolve_type_by_name(None, "e").is_some());
    assert!(db.resolve_type_by_name(None, "e2").is_none());
    assert!(db.resolve_type_by_name(None, "e3").is_none());
}

#[test]
fn built_in_objects_are_required_by_the_database_system() {
    // findDependentObjects refuses a pinned object before anything else,
    // CASCADE or not.
    assert_ddl_rejections(&[
        (
            "",
            "DROP FUNCTION int4pl(int4, int4);",
            "cannot drop function int4pl(integer,integer) because it is required by the \
             database system",
        ),
        (
            "",
            "DROP AGGREGATE sum(int4);",
            "cannot drop function sum(integer) because it is required by the database system",
        ),
        (
            "",
            "DROP OPERATOR + (int4, int4);",
            "cannot drop operator +(integer,integer) because it is required by the database \
             system",
        ),
        (
            "",
            "DROP TYPE int4 CASCADE;",
            "cannot drop type integer because it is required by the database system",
        ),
        (
            "",
            "DROP CAST (int4 AS int8) CASCADE;",
            "cannot drop cast from integer to bigint because it is required by the database \
             system",
        ),
        (
            "",
            "DROP CAST (int4range AS int4multirange);",
            "cannot drop cast from int4range to int4multirange because it is required by the \
             database system",
        ),
        (
            "",
            "DROP SCHEMA pg_catalog CASCADE;",
            "cannot drop schema pg_catalog because it is required by the database system",
        ),
    ]);
}

// ── What a stored INSERT writes ─────────────────────────────────────────────

#[test]
fn a_stored_insert_depends_on_every_column_it_writes() {
    // find_expr_references_walker: an INSERT depends on each target column
    // of its target list — without a column list, as many leading columns
    // as its source has values once `*` is expanded.
    let tables = "CREATE TABLE src (a int, b int);
                  CREATE TABLE dst (a int, b int, c int);";
    let body = |insert: &str| {
        format!(
            "{tables}
             CREATE FUNCTION f() RETURNS void LANGUAGE sql BEGIN ATOMIC {insert}; END;"
        )
    };
    let rule = |action: &str| {
        format!(
            "{tables}
             CREATE TABLE t (a int, b int);
             CREATE RULE r AS ON INSERT TO t DO ALSO {action};"
        )
    };
    let blocked = "cannot drop column b of table dst because other objects depend on it";
    let blocked_c = "cannot drop column c of table dst because other objects depend on it";
    let cases = [
        (body("INSERT INTO dst SELECT * FROM src"), "b"),
        (body("INSERT INTO dst SELECT s.*, 1 FROM src s"), "c"),
        (body("INSERT INTO dst TABLE src"), "b"),
        (
            body("WITH w AS (SELECT * FROM src) INSERT INTO dst SELECT * FROM w"),
            "b",
        ),
        (
            body("INSERT INTO dst SELECT 1, 2 UNION ALL SELECT 3, 4"),
            "b",
        ),
        (body("INSERT INTO dst VALUES (1, 2), (3, 4)"), "b"),
        (
            body("INSERT INTO dst VALUES ((SELECT s FROM src s LIMIT 1).*)"),
            "b",
        ),
        (rule("INSERT INTO dst VALUES (NEW.*)"), "b"),
        (rule("INSERT INTO dst VALUES (NEW.*, 1)"), "c"),
        (rule("INSERT INTO dst SELECT NEW.*"), "b"),
    ];
    for (setup, column) in &cases {
        let (stmt, msg) = if *column == "b" {
            ("ALTER TABLE dst DROP COLUMN b;", blocked)
        } else {
            ("ALTER TABLE dst DROP COLUMN c;", blocked_c)
        };
        assert_ddl_rejections(&[(setup, stmt, msg)]);
    }
    assert_ddl_rejections(&[(
        &rule("INSERT INTO dst VALUES (NEW.*)"),
        "ALTER TABLE dst ALTER COLUMN b TYPE bigint;",
        "cannot alter type of a column used by a view or rule",
    )]);
    // A dropped leading column takes no position.
    assert_ddl_rejections(&[(
        "CREATE TABLE src (a int, b int);
         CREATE TABLE dst (x int, a int, b int);
         ALTER TABLE dst DROP COLUMN x;
         CREATE FUNCTION f() RETURNS void LANGUAGE sql
             BEGIN ATOMIC INSERT INTO dst SELECT * FROM src; END;",
        "ALTER TABLE dst DROP COLUMN b;",
        blocked,
    )]);
    // The columns past the values, and DEFAULT VALUES, aren't written.
    for setup in [
        body("INSERT INTO dst VALUES (1, 2)"),
        body("INSERT INTO dst SELECT * FROM src"),
        body("INSERT INTO dst DEFAULT VALUES"),
        rule("INSERT INTO dst VALUES (NEW.*)"),
        rule("INSERT INTO dst DEFAULT VALUES"),
        rule("INSERT INTO dst VALUES (NEW.a, NEW.b), (NEW.b, 1)"),
    ] {
        build(&[
            ("0001.sql", &setup),
            ("0002.sql", "ALTER TABLE dst DROP COLUMN c;"),
        ]);
    }
}

#[test]
fn a_stored_insert_on_conflict_depends_on_what_it_infers() {
    // transformOnConflictClause: DO UPDATE reads EXCLUDED, a Var per column
    // of the target (exclRelTlist); the inference elements are Vars of
    // their columns; ON CONSTRAINT names the constraint
    // (OnConflictExpr.constraint).
    let body = |insert: &str| {
        format!(
            "CREATE TABLE dst (a int PRIMARY KEY, b int CONSTRAINT bu UNIQUE, c int);
             CREATE FUNCTION f() RETURNS void LANGUAGE sql BEGIN ATOMIC {insert}; END;"
        )
    };
    let rule = |action: &str| {
        format!(
            "CREATE TABLE dst (a int PRIMARY KEY, b int CONSTRAINT bu UNIQUE, c int);
             CREATE TABLE t (a int, b int);
             CREATE RULE r AS ON INSERT TO t DO ALSO {action};"
        )
    };
    let blocked = |column: &str| {
        format!("cannot drop column {column} of table dst because other objects depend on it")
    };
    let cases = [
        (
            body("INSERT INTO dst (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET b = 1"),
            "ALTER TABLE dst DROP COLUMN c;",
            blocked("c"),
        ),
        (
            rule("INSERT INTO dst (a) VALUES (NEW.a) ON CONFLICT (a) DO UPDATE SET b = 1"),
            "ALTER TABLE dst DROP COLUMN c;",
            blocked("c"),
        ),
        (
            body("INSERT INTO dst (a) VALUES (1) ON CONFLICT (b) DO NOTHING"),
            "ALTER TABLE dst DROP COLUMN b;",
            blocked("b"),
        ),
        (
            body("INSERT INTO dst (a) VALUES (1) ON CONFLICT ON CONSTRAINT bu DO NOTHING"),
            "ALTER TABLE dst DROP CONSTRAINT bu;",
            "cannot drop constraint bu on table dst because other objects depend on it".into(),
        ),
        // The column takes its UNIQUE constraint along, and that the
        // routine depends on.
        (
            body("INSERT INTO dst (a) VALUES (1) ON CONFLICT ON CONSTRAINT bu DO NOTHING"),
            "ALTER TABLE dst DROP COLUMN b;",
            blocked("b"),
        ),
    ];
    for (setup, stmt, msg) in &cases {
        assert_ddl_rejections(&[(setup, stmt, msg)]);
    }
    for (setup, stmt) in [
        (
            body("INSERT INTO dst (a) VALUES (1) ON CONFLICT (a) DO NOTHING"),
            "ALTER TABLE dst DROP COLUMN c;",
        ),
        (
            body("INSERT INTO dst (a) VALUES (1) ON CONFLICT ON CONSTRAINT bu DO NOTHING"),
            "ALTER TABLE dst DROP CONSTRAINT bu CASCADE;
             CREATE FUNCTION f() RETURNS int LANGUAGE sql RETURN 1;",
        ),
    ] {
        build(&[("0001.sql", &setup), ("0002.sql", stmt)]);
    }
}

// ── DROP SCHEMA: what depends on its objects, and on it ────────────────────

/// `sql` applied to a fresh catalog; `query` must then fail with `message`.
fn assert_gone(sql: &str, query: &str, message: &str) {
    let db = build_db(&[("0001.sql", sql)]);
    let err = db.analyze(query).unwrap_err();
    assert!(err.to_string().starts_with(message), "{query}: {err}");
}

#[test]
fn drop_schema_cascade_takes_what_uses_its_types() {
    // The row type of a composite type or of a table in the schema goes
    // with it, and so does what uses that type elsewhere.
    assert_gone(
        "CREATE SCHEMA s;
         CREATE TYPE s.ty AS (a int);
         CREATE VIEW v5 AS SELECT NULL::s.ty AS p;
         DROP SCHEMA s CASCADE;",
        "SELECT * FROM v5",
        "relation \"v5\" does not exist",
    );
    assert_gone(
        "CREATE SCHEMA s;
         CREATE TABLE s.tb (a int);
         CREATE VIEW v5 AS SELECT NULL::s.tb AS p;
         DROP SCHEMA s CASCADE;",
        "SELECT * FROM v5",
        "relation \"v5\" does not exist",
    );
    assert_gone(
        "CREATE SCHEMA s;
         CREATE TABLE s.tb (a int);
         CREATE FUNCTION f(s.tb) RETURNS int LANGUAGE sql AS 'SELECT 1';
         DROP SCHEMA s CASCADE;",
        "SELECT f(NULL)",
        "function f(unknown) does not exist",
    );
    assert_gone(
        "CREATE SCHEMA s;
         CREATE TABLE s.tb (a int);
         CREATE FUNCTION g() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT (NULL::s.tb).a; END;
         DROP SCHEMA s CASCADE;",
        "SELECT g()",
        "function g() does not exist",
    );
    // A column of the type in another schema's table goes; the table stays.
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA s;
         CREATE TABLE s.tb (a int);
         CREATE TABLE t2 (x s.tb, y int);
         DROP SCHEMA s CASCADE;",
    )]);
    let q = db.analyze("SELECT * FROM t2").unwrap();
    assert_eq!(q.columns.len(), 1);
    // A table's row type pins it like the table itself.
    assert_ddl_rejections(&[
        (
            "CREATE TABLE tb (a int);
             CREATE FUNCTION g() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT (NULL::tb).a; END;",
            "DROP TABLE tb;",
            "cannot drop table tb because other objects depend on it",
        ),
        (
            "CREATE TABLE tb (a int);
             CREATE TABLE t2 (b int DEFAULT (NULL::tb).a);",
            "DROP TABLE tb;",
            "cannot drop table tb because other objects depend on it",
        ),
    ]);
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE tb (a int);
         CREATE TABLE t2 (b int DEFAULT (NULL::tb).a);
         DROP TABLE tb CASCADE;
         INSERT INTO t2 DEFAULT VALUES;",
    )]);
    assert!(db.resolve_table(None, "tb").is_none());
}

#[test]
fn drop_schema_honours_what_refers_to_it() {
    // A stored `regnamespace` constant depends on its schema.
    assert_ddl_rejections(&[
        (
            "CREATE SCHEMA s;
             CREATE VIEW v6 AS SELECT 's'::regnamespace AS p;",
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it",
        ),
        (
            "CREATE SCHEMA s;
             CREATE TABLE t (a int DEFAULT ('s'::regnamespace)::oid::int);",
            "DROP SCHEMA s RESTRICT;",
            "cannot drop schema s because other objects depend on it",
        ),
        // `public` isn't pinned: the dependency is recorded.
        (
            "CREATE VIEW v6 AS SELECT 'public'::regnamespace AS p;",
            "DROP SCHEMA public;",
            "cannot drop schema public because other objects depend on it",
        ),
        // So does everything in the schema.
        (
            "CREATE SCHEMA s; CREATE COLLATION s.co (locale = 'C');",
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it",
        ),
        (
            "CREATE SCHEMA s; CREATE TEXT SEARCH CONFIGURATION s.c (COPY = simple);",
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it",
        ),
        (
            "CREATE SCHEMA s;
             CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'SELECT 1';
             CREATE OPERATOR s.### (rightarg = int, function = f);",
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it",
        ),
    ]);
    assert_gone(
        "CREATE SCHEMA s;
         CREATE VIEW v6 AS SELECT 's'::regnamespace AS p;
         DROP SCHEMA s CASCADE;",
        "SELECT * FROM v6",
        "relation \"v6\" does not exist",
    );
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA s;
         CREATE TABLE t (a int DEFAULT ('s'::regnamespace)::oid::int);
         DROP SCHEMA s CASCADE;
         INSERT INTO t DEFAULT VALUES;",
    )]);
    assert!(db.resolve_table(None, "t").is_some());
    // Cast from text, the name is looked up when the view runs: no
    // dependency.
    build(&[(
        "0001.sql",
        "CREATE SCHEMA s;
         CREATE VIEW v6 AS SELECT 's'::text::regnamespace AS p;
         DROP SCHEMA s;",
    )]);
    // An operator of the schema goes with it.
    assert_gone(
        "CREATE SCHEMA s;
         CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE OPERATOR s.### (rightarg = int, function = f);
         DROP SCHEMA s CASCADE;",
        "SELECT OPERATOR(s.###) 1",
        "schema \"s\" does not exist",
    );
}

// ── DROP COLLATION ─────────────────────────────────────────────────────────

#[test]
fn drop_collation_honours_what_uses_it() {
    let blocked = "cannot drop collation co because other objects depend on it";
    let co = "CREATE COLLATION co (locale = 'C');";
    let cases: Vec<(String, &str, &str)> = vec![
        (
            format!("{co} CREATE TABLE t (a text COLLATE co);"),
            "DROP COLLATION co;",
            blocked,
        ),
        (
            format!("{co} CREATE TYPE ct AS (a text COLLATE co, b int);"),
            "DROP COLLATION co;",
            blocked,
        ),
        (
            format!("{co} CREATE VIEW v AS SELECT 'a'::text COLLATE co AS x;"),
            "DROP COLLATION co;",
            blocked,
        ),
        (
            format!("{co} CREATE VIEW v AS SELECT 'co'::regcollation AS x;"),
            "DROP COLLATION co RESTRICT;",
            blocked,
        ),
        (
            format!("{co} CREATE DOMAIN d AS text COLLATE co;"),
            "DROP COLLATION co;",
            blocked,
        ),
        (
            format!("{co} CREATE TABLE t (a text); CREATE INDEX i ON t (a COLLATE co);"),
            "DROP COLLATION co;",
            blocked,
        ),
        (
            format!("{co} CREATE TYPE r AS RANGE (subtype = text, collation = co);"),
            "DROP COLLATION co;",
            blocked,
        ),
        (
            format!("{co} CREATE TABLE t (a text CHECK (a > 'x' COLLATE co));"),
            "DROP COLLATION co;",
            blocked,
        ),
        (
            format!(
                "{co} CREATE FUNCTION f() RETURNS text LANGUAGE sql
                     BEGIN ATOMIC SELECT 'a' COLLATE co; END;"
            ),
            "DROP COLLATION co;",
            blocked,
        ),
        (
            "CREATE SCHEMA s; CREATE COLLATION s.co (locale = 'C');
             CREATE TABLE t (a text COLLATE s.co);"
                .into(),
            "DROP COLLATION s.co;",
            "cannot drop collation s.co because other objects depend on it",
        ),
        (
            "CREATE SCHEMA s; CREATE COLLATION s.co (locale = 'C');
             CREATE TABLE t (a text COLLATE s.co);"
                .into(),
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it",
        ),
        (
            "".into(),
            "DROP COLLATION nope;",
            "collation \"nope\" for encoding \"UTF8\" does not exist",
        ),
        (
            "".into(),
            "DROP COLLATION s.nope;",
            "schema \"s\" does not exist",
        ),
        (
            "".into(),
            "DROP COLLATION \"C\";",
            "cannot drop collation \"C\" because it is required by the database system",
        ),
    ];
    for (setup, stmt, msg) in &cases {
        assert_ddl_rejections(&[(setup, stmt, msg)]);
    }

    // Without dependents, or with CASCADE, it goes — with them.
    build(&[(
        "0001.sql",
        "CREATE COLLATION co (locale = 'C');
         CREATE VIEW v AS SELECT 'co'::text::regcollation AS x;
         DROP COLLATION co;
         DROP COLLATION IF EXISTS co;
         DROP COLLATION IF EXISTS s.nope;
         CREATE COLLATION co (locale = 'C');",
    )]);
    let db = build_db(&[(
        "0001.sql",
        "CREATE COLLATION co (locale = 'C');
         CREATE TABLE t (a text COLLATE co, b int);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE INDEX ON t (a);
         CREATE TABLE pa (a text COLLATE co, c int);
         CREATE TABLE ch (b int) INHERITS (pa);
         CREATE TYPE ct AS (a text COLLATE co, b int);
         CREATE DOMAIN d AS text COLLATE co;
         CREATE TABLE td (x d, y int);
         CREATE TABLE ti (a text, b int);
         CREATE INDEX i ON ti (a COLLATE co);
         CREATE TYPE r AS RANGE (subtype = text, collation = co);
         CREATE TABLE p (a text COLLATE co, b int) PARTITION BY LIST (a);
         CREATE TABLE p1 PARTITION OF p FOR VALUES IN ('x');
         CREATE TABLE q (a text COLLATE co, b int) PARTITION BY LIST (b);
         CREATE TABLE q1 PARTITION OF q FOR VALUES IN (1);
         CREATE VIEW vc AS SELECT 'a'::text COLLATE co AS x;
         DROP COLLATION co CASCADE;",
    )]);
    for (query, columns) in [
        ("SELECT * FROM t", 1),
        ("SELECT * FROM pa", 1),
        ("SELECT * FROM ch", 2),
        ("SELECT (NULL::ct).*", 1),
        ("SELECT * FROM td", 1),
        ("SELECT * FROM ti", 2),
        ("SELECT * FROM q", 1),
        ("SELECT * FROM q1", 1),
    ] {
        let q = db.analyze(query).unwrap();
        assert_eq!(q.columns.len(), columns, "{query}");
    }
    for (query, message) in [
        ("SELECT * FROM v", "relation \"v\" does not exist"),
        ("SELECT * FROM vc", "relation \"vc\" does not exist"),
        ("SELECT * FROM p", "relation \"p\" does not exist"),
        ("SELECT * FROM p1", "relation \"p1\" does not exist"),
        ("SELECT NULL::d", "type \"d\" does not exist"),
        ("SELECT NULL::r", "type \"r\" does not exist"),
        (
            "SELECT 'a' COLLATE co",
            "collation \"co\" for encoding \"UTF8\" does not exist",
        ),
    ] {
        let err = db.analyze(query).unwrap_err();
        assert!(err.to_string().starts_with(message), "{query}: {err}");
    }
    assert!(db.resolve_table(None, "i").is_none());

    // DROP SCHEMA ... CASCADE takes its collation, and what uses it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA s;
         CREATE COLLATION s.co (locale = 'C');
         CREATE TABLE t (a text COLLATE s.co, b int);
         CREATE VIEW v AS SELECT 'a'::text COLLATE s.co AS x;
         DROP SCHEMA s CASCADE;",
    )]);
    assert_eq!(db.analyze("SELECT * FROM t").unwrap().columns.len(), 1);
    assert!(db.resolve_table(None, "v").is_none());
}

#[test]
fn alter_collation_renames_and_moves_it() {
    // AlterObjectRename_internal / AlterObjectNamespace_internal: the
    // collation keeps its OID, so what uses it follows it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE COLLATION co (locale = 'C');
         ALTER COLLATION co RENAME TO co2;
         CREATE COLLATION co (provider = icu, locale = 'und');
         DROP COLLATION co;
         CREATE SCHEMA s;
         ALTER COLLATION co2 SET SCHEMA s;
         ALTER COLLATION s.co2 SET SCHEMA s;
         CREATE TABLE t (a text COLLATE s.co2);
         CREATE VIEW v AS SELECT 'a'::text COLLATE s.co2 AS x;
         ALTER COLLATION s.co2 RENAME TO co3;
         ALTER COLLATION s.co3 SET SCHEMA public;
         ALTER COLLATION co3 REFRESH VERSION;
         COMMENT ON COLLATION co3 IS 'c';",
    )]);
    assert_eq!(db.analyze("SELECT * FROM v").unwrap().columns.len(), 1);
    db.analyze("SELECT a COLLATE co3 FROM t").unwrap();
    for query in ["SELECT 'a' COLLATE co2", "SELECT 'a' COLLATE s.co3"] {
        let err = db.analyze(query).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{query}: {err}");
    }
    build(&[(
        "0001.sql",
        "CREATE COLLATION co (locale = 'C');
         ALTER COLLATION co RENAME TO co2;
         DROP COLLATION co2;
         CREATE SCHEMA s;
         CREATE COLLATION co (locale = 'C');
         ALTER COLLATION co SET SCHEMA s;
         DROP COLLATION s.co;
         CREATE COLLATION co (locale = 'C');
         CREATE TABLE t (a text COLLATE co);
         ALTER COLLATION co RENAME TO co2;
         DROP COLLATION co2 CASCADE;",
    )]);

    let two = "CREATE COLLATION c1 (locale = 'C');
               CREATE COLLATION c2 (provider = icu, locale = 'und');
               CREATE SCHEMA s;
               CREATE COLLATION s.c1 (provider = icu, locale = 'und');
               CREATE COLLATION s.c2 (locale = 'C');";
    assert_ddl_rejections(&[
        (
            two,
            "ALTER COLLATION c2 RENAME TO c1;",
            "collation \"c1\" for encoding \"UTF8\" already exists in schema \"public\"",
        ),
        (
            two,
            "ALTER COLLATION c1 RENAME TO c2;",
            "collation \"c2\" already exists in schema \"public\"",
        ),
        (
            two,
            "ALTER COLLATION c1 SET SCHEMA s;",
            "collation \"c1\" already exists in schema \"s\"",
        ),
        (
            two,
            "ALTER COLLATION c2 SET SCHEMA s;",
            "collation \"c2\" for encoding \"UTF8\" already exists in schema \"s\"",
        ),
        (
            two,
            "CREATE COLLATION c1 (locale = 'C');",
            "collation \"c1\" for encoding \"UTF8\" already exists",
        ),
        (
            two,
            "CREATE COLLATION c2 (locale = 'C');",
            "collation \"c2\" already exists",
        ),
        (
            two,
            "CREATE COLLATION c1 (provider = icu, locale = 'und');",
            "collation \"c1\" already exists",
        ),
        (
            two,
            "CREATE COLLATION c1 FROM \"C\";",
            "collation \"c1\" already exists",
        ),
        (
            two,
            "ALTER COLLATION c1 SET SCHEMA pg_temp;",
            "cannot move objects into or out of temporary schemas",
        ),
        (
            two,
            "ALTER COLLATION c1 SET SCHEMA pg_toast;",
            "cannot move objects into or out of TOAST schema",
        ),
        (
            two,
            "ALTER COLLATION c1 SET SCHEMA nosch;",
            "schema \"nosch\" does not exist",
        ),
        (
            "",
            "ALTER COLLATION nope SET SCHEMA nosch;",
            "collation \"nope\" for encoding \"UTF8\" does not exist",
        ),
        (
            "",
            "ALTER COLLATION nope RENAME TO x;",
            "collation \"nope\" for encoding \"UTF8\" does not exist",
        ),
        (
            "",
            "ALTER COLLATION public.nope RENAME TO x;",
            "collation \"public.nope\" for encoding \"UTF8\" does not exist",
        ),
        (
            "",
            "ALTER COLLATION nosch.nope RENAME TO x;",
            "schema \"nosch\" does not exist",
        ),
        (
            "",
            "ALTER COLLATION nope REFRESH VERSION;",
            "collation \"nope\" for encoding \"UTF8\" does not exist",
        ),
        (
            "",
            "ALTER COLLATION nope OWNER TO CURRENT_USER;",
            "collation \"nope\" for encoding \"UTF8\" does not exist",
        ),
        (
            "",
            "COMMENT ON COLLATION nosch.nope IS 'x';",
            "schema \"nosch\" does not exist",
        ),
    ]);
}

#[test]
fn a_stored_reg_constant_pins_what_it_names() {
    // find_expr_references_walker: a regproc / regprocedure / regoper /
    // regoperator / regtype / regclass constant depends on its object.
    let f = "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'SELECT 1';";
    let op = "CREATE FUNCTION f2(int, int) RETURNS int LANGUAGE sql AS 'SELECT 1';
              CREATE OPERATOR ### (leftarg = int, rightarg = int, function = f2);";
    let cases: Vec<(String, &str, &str)> = vec![
        (
            format!("{f} CREATE VIEW v AS SELECT 'f'::regproc AS p;"),
            "DROP FUNCTION f(int);",
            "cannot drop function f(integer) because other objects depend on it",
        ),
        (
            format!("{f} CREATE VIEW v AS SELECT 'f(int)'::regprocedure AS p;"),
            "DROP FUNCTION f(int);",
            "cannot drop function f(integer) because other objects depend on it",
        ),
        (
            format!("{op} CREATE VIEW v AS SELECT '###'::regoper AS p;"),
            "DROP OPERATOR ###(int, int);",
            "cannot drop operator ###(integer,integer) because other objects depend on it",
        ),
        (
            format!("{op} CREATE VIEW v AS SELECT '###(int,int)'::regoperator AS p;"),
            "DROP OPERATOR ###(int, int);",
            "cannot drop operator ###(integer,integer) because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS (a int); CREATE VIEW v AS SELECT 'ty'::regtype AS p;".into(),
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        // On the array type, which is part of its element type.
        (
            "CREATE TYPE ty AS (a int); CREATE VIEW v AS SELECT 'ty[]'::regtype AS p;".into(),
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            "CREATE TABLE tt (a int); CREATE VIEW v AS SELECT '_tt'::regtype AS p;".into(),
            "DROP TABLE tt;",
            "cannot drop table tt because other objects depend on it",
        ),
        // An untyped literal coerced to the reg type is a constant too.
        (
            "CREATE TYPE ty AS (a int); CREATE TABLE t (a regtype DEFAULT 'ty');".into(),
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            format!("{f} CREATE TABLE t (a regproc DEFAULT 'f');"),
            "DROP FUNCTION f(int);",
            "cannot drop function f(integer) because other objects depend on it",
        ),
        (
            format!("{f} CREATE TABLE t (a regprocedure DEFAULT 'f(int)');"),
            "DROP FUNCTION f(int);",
            "cannot drop function f(integer) because other objects depend on it",
        ),
        (
            "CREATE COLLATION co (locale = 'C'); CREATE TABLE t (a regcollation DEFAULT 'co');"
                .into(),
            "DROP COLLATION co;",
            "cannot drop collation co because other objects depend on it",
        ),
        (
            "CREATE SCHEMA s; CREATE TABLE t (a regnamespace DEFAULT 's');".into(),
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS (a int); CREATE DOMAIN dr AS regtype;
             CREATE TABLE t (a dr DEFAULT 'ty');"
                .into(),
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS (a int); CREATE TABLE t (a regtype);
             ALTER TABLE t ALTER a SET DEFAULT 'ty';"
                .into(),
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS (a int);
             CREATE TABLE t (a regtype GENERATED ALWAYS AS ('ty') STORED);"
                .into(),
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS (a int); CREATE DOMAIN d AS regtype DEFAULT 'ty';".into(),
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS (a int); CREATE VIEW v AS SELECT COALESCE(NULL::regtype, 'ty') AS x;"
                .into(),
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            "CREATE SCHEMA s; CREATE TABLE t (a regnamespace); CREATE TABLE u (b int);
             CREATE RULE r AS ON INSERT TO u DO ALSO INSERT INTO t VALUES ('s');"
                .into(),
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it",
        ),
    ];
    for (setup, stmt, msg) in &cases {
        assert_ddl_rejections(&[(setup, stmt, msg)]);
    }
    // With CASCADE the view goes; a name cast from text is looked up when
    // the view runs and pins nothing.
    for (setup, stmt) in [
        (
            format!("{f} CREATE VIEW v AS SELECT 'f(int)'::regprocedure AS p;"),
            "DROP FUNCTION f(int) CASCADE;",
        ),
        (
            format!("{op} CREATE VIEW v AS SELECT '###(int,int)'::regoperator AS p;"),
            "DROP OPERATOR ###(int, int) CASCADE;",
        ),
        (
            "CREATE TYPE ty AS (a int); CREATE VIEW v AS SELECT 'ty[]'::regtype AS p;".into(),
            "DROP TYPE ty CASCADE;",
        ),
        (
            format!(
                "{f} CREATE VIEW w AS SELECT 'f'::text::regproc AS p; CREATE VIEW v AS SELECT 1 AS p;"
            ),
            "DROP FUNCTION f(int); DROP VIEW v;",
        ),
        // Nor does an array of them: the dependency is on a scalar
        // constant only.
        (
            "CREATE TYPE ty AS (a int); CREATE TABLE t (a regtype[] DEFAULT '{ty}');
             CREATE VIEW w AS SELECT '{ty}'::regtype[] AS p; CREATE VIEW v AS SELECT 1 AS p;"
                .into(),
            "DROP TYPE ty; DROP VIEW v;",
        ),
    ] {
        assert_gone(
            &format!("{setup} {stmt}"),
            "SELECT * FROM v",
            "relation \"v\" does not exist",
        );
    }
}

#[test]
fn what_a_table_owns_may_use_its_row_type() {
    // A table's own CHECK constraints, indexes and policies go with it,
    // whatever they refer to of it — its row type and that type's array
    // type included.
    build(&[(
        "0001.sql",
        "CREATE TABLE tb (a int CHECK (ROW(a)::tb IS NOT NULL));
         CREATE POLICY p ON tb USING ((NULL::tb).a IS NULL);
         DROP TABLE tb;
         CREATE TABLE tt (a int, CHECK ('tt[]'::regtype IS NOT NULL),
                          CHECK ('tt'::regtype IS NOT NULL));
         CREATE INDEX ON tt (('tt'::regtype::oid));
         CREATE INDEX ON tt (a) WHERE 'tt[]'::regtype IS NOT NULL;
         DROP TABLE tt;
         CREATE TABLE tt (b int);
         CREATE INDEX tt_oid_idx ON tt (b);",
    )]);
    // Another table's are not its own.
    assert_ddl_rejections(&[
        (
            "CREATE TABLE tt (a int); CREATE TABLE o (a int, CHECK ('tt'::regtype IS NOT NULL));",
            "DROP TABLE tt;",
            "cannot drop table tt because other objects depend on it",
        ),
        (
            "CREATE TABLE tt (a int); CREATE TABLE o (a int);
             CREATE INDEX ON o (('tt[]'::regtype::oid));",
            "DROP TABLE tt;",
            "cannot drop table tt because other objects depend on it",
        ),
    ]);
}

#[test]
fn a_domain_depends_on_what_its_default_uses() {
    // GenerateTypeDependencies: a domain depends on its default's
    // objects; ALTER DOMAIN ... SET / DROP DEFAULT records them anew.
    let f = "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT 1';";
    assert_ddl_rejections(&[
        (
            &format!("{f} CREATE DOMAIN d AS int DEFAULT f();"),
            "DROP FUNCTION f();",
            "cannot drop function f() because other objects depend on it",
        ),
        (
            &format!("{f} CREATE DOMAIN d AS int; ALTER DOMAIN d SET DEFAULT f();"),
            "DROP FUNCTION f();",
            "cannot drop function f() because other objects depend on it",
        ),
        // The base type stays depended on.
        (
            "CREATE TYPE ty AS (a int); CREATE DOMAIN d AS ty DEFAULT NULL;
             ALTER DOMAIN d SET DEFAULT NULL; ALTER DOMAIN d DROP DEFAULT;",
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
    ]);
    build(&[(
        "0001.sql",
        &format!(
            "{f} CREATE DOMAIN d AS int DEFAULT f(); ALTER DOMAIN d SET DEFAULT 1;
             DROP FUNCTION f();
             {f} ALTER DOMAIN d SET DEFAULT f(); ALTER DOMAIN d DROP DEFAULT;
             DROP FUNCTION f();"
        ),
    )]);
    // With CASCADE the domain goes, and the columns of it.
    let db = build_db(&[(
        "0001.sql",
        &format!(
            "{f} CREATE DOMAIN d AS int DEFAULT f(); CREATE TABLE t (a d, b int);
             DROP FUNCTION f() CASCADE;"
        ),
    )]);
    assert_eq!(db.analyze("SELECT * FROM t").unwrap().columns.len(), 1);
    let err = db.analyze("SELECT NULL::d").unwrap_err();
    assert!(
        err.to_string().starts_with("type \"d\" does not exist"),
        "{err}"
    );
}
