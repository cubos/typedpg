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
        "cannot drop type status because other objects depend on it (column s of table t depends on type status)"
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
fn reg_literals_are_dependencies_of_stored_expressions() {
    // find_expr_references_walker records a dependency on the object each
    // reg* constant names — in a view, a default (whose untyped literal is
    // input as the column's type) or a generation expression. PG 18: 2BP01
    // cannot drop ... because other objects depend on it.
    for (setup, drop, msg) in [
        (
            "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE VIEW v AS SELECT 'f(int)'::regprocedure AS p;",
            "DROP FUNCTION f(int);",
            "cannot drop function f(integer) because other objects depend on it",
        ),
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'select 1';
             CREATE VIEW v AS SELECT 'f'::regproc AS p;",
            "DROP FUNCTION f();",
            "cannot drop function f() because other objects depend on it",
        ),
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'select 1';
             CREATE TABLE t (b regproc);
             ALTER TABLE t ALTER b SET DEFAULT 'f';",
            "DROP FUNCTION f();",
            "cannot drop function f() because other objects depend on it",
        ),
        (
            "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE TABLE t (b regprocedure GENERATED ALWAYS AS ('f(int)') STORED);",
            "DROP FUNCTION f(int);",
            "cannot drop function f(integer) because other objects depend on it",
        ),
        (
            "CREATE FUNCTION eqx(int, int) RETURNS bool LANGUAGE sql AS 'select $1 = $2';
             CREATE OPERATOR === (leftarg = int, rightarg = int, function = eqx);
             CREATE VIEW v AS SELECT '===(int,int)'::regoperator AS p;",
            "DROP OPERATOR === (int, int);",
            "cannot drop operator ===(integer,integer) because other objects depend on it",
        ),
        (
            "CREATE FUNCTION eqx(int, int) RETURNS bool LANGUAGE sql AS 'select $1 = $2';
             CREATE OPERATOR === (leftarg = int, rightarg = int, function = eqx);
             CREATE TABLE t (o regoper DEFAULT '===');",
            "DROP OPERATOR === (int, int);",
            "cannot drop operator ===(integer,integer) because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS (a int);
             CREATE VIEW v AS SELECT 'ty'::regtype AS p;",
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        // The array type is internal to its element type: what depends on
        // it depends on the type.
        (
            "CREATE TYPE ty AS (a int);
             CREATE VIEW v AS SELECT 'ty[]'::regtype AS p;",
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS (a int);
             CREATE TABLE t (b regtype DEFAULT '_ty');",
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS (a int);
             CREATE VIEW v AS SELECT 1 AS p WHERE NULL::_ty IS NULL;",
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
        (
            "CREATE TYPE ty AS ENUM ('a');
             CREATE TABLE t (a int, CHECK (NULL::_ty IS NULL));",
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", drop)]).expect_err(setup);
        assert!(err.to_string().starts_with(msg), "{setup}\n  got: {err}");
    }
    // CASCADE takes the dependents along.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TYPE ty AS (a int);
         CREATE VIEW v AS SELECT 'ty[]'::regtype AS p;
         CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE VIEW w AS SELECT 'f(int)'::regprocedure AS p;
         DROP TYPE ty CASCADE;
         DROP FUNCTION f(int) CASCADE;",
    )]);
    assert!(db.resolve_table(None, "v").is_none());
    assert!(db.resolve_table(None, "w").is_none());
    // Built-in objects are pinned: nothing is recorded on them.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (b regtype DEFAULT 'int4', p regprocedure DEFAULT 'abs(int)');
         CREATE VIEW v AS SELECT '+(int,int)'::regoperator AS o;
         DROP VIEW v;
         DROP TABLE t;",
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

/// `stmt` after `setup` fails, its message starting with `message` (PG's
/// errmsg, then its DETAIL in parentheses).
fn assert_drop_fails(setup: &str, stmt: &str, message: &str) {
    let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)])
        .expect_err(&format!("{setup}\n{stmt}"));
    assert!(
        err.to_string().starts_with(message),
        "{setup}\n{stmt}\n  got: {err}"
    );
}

#[test]
fn a_text_search_dictionary_depends_on_its_template() {
    let template =
        "CREATE TEXT SEARCH TEMPLATE mytmpl (lexize = dsimple_lexize, init = dsimple_init);
                    CREATE TEXT SEARCH DICTIONARY d2 (template = mytmpl);";
    assert_drop_fails(
        template,
        "DROP TEXT SEARCH TEMPLATE mytmpl;",
        "cannot drop text search template mytmpl because other objects depend on it (text \
         search dictionary d2 depends on text search template mytmpl)",
    );
    build(&[
        ("0001.sql", template),
        ("0002.sql", "DROP TEXT SEARCH TEMPLATE mytmpl CASCADE;"),
        ("0003.sql", template),
    ]);
    // A configuration depends on the dictionaries it maps to.
    assert_drop_fails(
        "CREATE TEXT SEARCH DICTIONARY d3 (template = simple);
         CREATE TEXT SEARCH CONFIGURATION c3 (copy = english);
         ALTER TEXT SEARCH CONFIGURATION c3 ALTER MAPPING FOR asciiword WITH d3;",
        "DROP TEXT SEARCH DICTIONARY d3;",
        "cannot drop text search dictionary d3 because other objects depend on it (text search \
         configuration c3 depends on text search dictionary d3)",
    );
}

#[test]
fn an_operator_class_holds_on_to_its_operators() {
    let setup =
        "CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
                 CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
                 CREATE OPERATOR CLASS c FOR TYPE int USING hash AS
                     OPERATOR 1 ===(int, int), FUNCTION 1 hashint4(int);";
    assert_drop_fails(
        setup,
        "DROP OPERATOR ===(int, int);",
        "cannot drop operator ===(integer,integer) because other objects depend on it (operator \
         class c for access method hash depends on operator ===(integer,integer))",
    );
    assert_drop_fails(
        setup,
        "DROP FUNCTION f(int, int);",
        "cannot drop function f(integer,integer) because other objects depend on it",
    );
    // CASCADE takes the class along; creating both again works.
    build(&[
        ("0001.sql", setup),
        ("0002.sql", "DROP OPERATOR ===(int, int) CASCADE;"),
        (
            "0003.sql",
            "CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
             CREATE OPERATOR CLASS c FOR TYPE int USING hash AS
                 OPERATOR 1 ===(int, int), FUNCTION 1 hashint4(int);",
        ),
    ]);
    // A family's loose member goes with its operator, without CASCADE.
    build(&[(
        "0001.sql",
        "CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
         CREATE OPERATOR FAMILY fam USING hash;
         ALTER OPERATOR FAMILY fam USING hash ADD OPERATOR 1 ===(int, int);
         DROP OPERATOR ===(int, int);
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
         ALTER OPERATOR FAMILY fam USING hash ADD OPERATOR 1 ===(int, int);",
    )]);
}

#[test]
fn a_drop_naming_several_objects_reports_them_together() {
    // RemoveRelations checks each name's relkind before any dependency.
    for stmt in ["DROP VIEW v1, a;", "DROP VIEW a, v1;"] {
        assert_drop_fails(
            "CREATE TABLE a (x int); CREATE VIEW v1 AS SELECT 1;",
            stmt,
            "\"a\" is not a view",
        );
    }
    assert_drop_fails(
        "CREATE SCHEMA s1; CREATE SCHEMA s2; CREATE TABLE s1.t (x int);",
        "DROP SCHEMA s1, s2;",
        "cannot drop desired object(s) because other objects depend on them (table s1.t \
         depends on schema s1)",
    );
    assert_drop_fails(
        "CREATE SCHEMA s1; CREATE TABLE s1.t (x int);",
        "DROP SCHEMA s1;",
        "cannot drop schema s1 because other objects depend on it (table s1.t depends on schema \
         s1)",
    );
    assert_drop_fails(
        "CREATE TABLE a (x int); CREATE TABLE b (x int); CREATE VIEW v AS SELECT * FROM a;",
        "DROP TABLE b, a;",
        "cannot drop desired object(s) because other objects depend on them (view v depends on \
         table a)",
    );
    assert_drop_fails(
        "CREATE TYPE e AS ENUM ('a'); CREATE TABLE t (c e); CREATE TYPE e2 AS ENUM ('b');",
        "DROP TYPE e2, e;",
        "cannot drop desired object(s) because other objects depend on them (column c of table \
         t depends on type e)",
    );
}

#[test]
fn dependency_details_name_the_dependent_as_pg_does() {
    assert_drop_fails(
        "CREATE DOMAIN d AS int; CREATE TABLE t (c d);",
        "DROP DOMAIN d;",
        "cannot drop type d because other objects depend on it (column c of table t depends on \
         type d)",
    );
    assert_drop_fails(
        "CREATE TYPE e AS ENUM ('a'); CREATE TABLE t (c e);",
        "DROP TYPE e;",
        "cannot drop type e because other objects depend on it (column c of table t depends on \
         type e)",
    );
    for (key, constraint) in [("PRIMARY KEY", "t_pkey"), ("UNIQUE", "t_id_key")] {
        assert_drop_fails(
            &format!("CREATE TABLE t (id int {key}); CREATE TABLE c (x int REFERENCES t (id));"),
            &format!("ALTER TABLE t DROP CONSTRAINT {constraint};"),
            &format!(
                "cannot drop constraint {constraint} on table t because other objects depend on \
                 it (constraint c_x_fkey on table c depends on index {constraint})"
            ),
        );
    }
}

// ── Operator family members (amadjustmembers) ───────────────────────────────

const EQ_INT: &str =
    "CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
                      CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);";
const EQ_BIGINT: &str =
    "CREATE FUNCTION fb(bigint, bigint) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
                         CREATE OPERATOR === (leftarg = bigint, rightarg = bigint, function = fb);";

#[test]
fn a_same_type_member_is_tied_to_the_familys_class_for_its_type() {
    // No bigint class in the family: the operator is a loose member, and
    // goes with its operator.
    for class in [
        "CREATE OPERATOR CLASS oc FOR TYPE int USING btree AS
             OPERATOR 3 ===(bigint, bigint), FUNCTION 1 btint4cmp(int, int);",
        "CREATE OPERATOR CLASS oc FOR TYPE int USING hash AS
             OPERATOR 1 ===(bigint, bigint), FUNCTION 1 hashint4(int);",
    ] {
        build(&[
            ("0001.sql", EQ_BIGINT),
            ("0002.sql", class),
            ("0003.sql", "DROP OPERATOR ===(bigint, bigint);"),
        ]);
    }
    // A bigint class in the family holds it.
    assert_drop_fails(
        &format!(
            "{EQ_BIGINT}
             CREATE OPERATOR CLASS oc FOR TYPE int USING btree AS FUNCTION 1 btint4cmp(int, int);
             CREATE OPERATOR CLASS oc8 FOR TYPE bigint USING btree FAMILY oc AS
                 FUNCTION 1 btint8cmp(bigint, bigint);
             CREATE OPERATOR CLASS oc2 FOR TYPE int USING btree FAMILY oc AS
                 OPERATOR 3 ===(bigint, bigint);"
        ),
        "DROP OPERATOR ===(bigint, bigint);",
        "cannot drop operator ===(bigint,bigint) because other objects depend on it (operator \
         class oc8 for access method btree depends on operator ===(bigint,bigint))",
    );
}

#[test]
fn alter_operator_family_add_ties_a_member_to_the_class_for_its_type() {
    let setup =
        "CREATE FUNCTION lt(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 < $2';
                 CREATE OPERATOR <<< (leftarg = int, rightarg = int, function = lt);
                 CREATE OPERATOR FAMILY fam USING btree;
                 CREATE OPERATOR CLASS oc FOR TYPE int USING btree FAMILY fam AS
                     FUNCTION 1 btint4cmp(int, int);
                 ALTER OPERATOR FAMILY fam USING btree ADD OPERATOR 1 <<<(int, int);";
    assert_drop_fails(
        setup,
        "DROP OPERATOR <<<(int, int);",
        "cannot drop operator <<<(integer,integer) because other objects depend on it (operator \
         class oc for access method btree depends on operator <<<(integer,integer))",
    );
    // CASCADE takes the class; creating it again works.
    build(&[
        ("0001.sql", setup),
        ("0002.sql", "DROP OPERATOR <<<(int, int) CASCADE;"),
        (
            "0003.sql",
            "CREATE OPERATOR CLASS oc FOR TYPE int USING btree FAMILY fam AS
                 FUNCTION 1 btint4cmp(int, int);",
        ),
    ]);
    // Without a class for int, the member is loose.
    build(&[(
        "0001.sql",
        "CREATE FUNCTION lt(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 < $2';
         CREATE OPERATOR <<< (leftarg = int, rightarg = int, function = lt);
         CREATE OPERATOR FAMILY fam USING btree;
         ALTER OPERATOR FAMILY fam USING btree ADD OPERATOR 1 <<<(int, int);
         DROP OPERATOR <<<(int, int);",
    )]);
}

#[test]
fn a_member_tied_to_a_class_goes_only_with_it() {
    for (am, class, stmt, member) in [
        (
            "btree",
            "OPERATOR 3 ===(int, int), FUNCTION 1 btint4cmp(int, int)",
            "DROP OPERATOR 3 (int, int)",
            "operator 3 (integer, integer) of operator family fam for access method btree: \
             ===(integer,integer)",
        ),
        (
            "btree",
            "OPERATOR 3 ===(int, int), FUNCTION 1 btint4cmp(int, int)",
            "DROP FUNCTION 1 (int, int)",
            "function 1 (integer, integer) of operator family fam for access method btree: \
             btint4cmp(integer,integer)",
        ),
        (
            "brin",
            "OPERATOR 3 ===(int, int)",
            "DROP OPERATOR 3 (int, int)",
            "operator 3 (integer, integer) of operator family fam for access method brin: \
             ===(integer,integer)",
        ),
        (
            "bloom",
            "OPERATOR 1 ===(int, int), FUNCTION 1 hashint4(int)",
            "DROP OPERATOR 1 (int, int)",
            "operator 1 (integer, integer) of operator family fam for access method bloom: \
             ===(integer,integer)",
        ),
    ] {
        let setup = format!(
            "CREATE EXTENSION bloom; {EQ_INT}
             CREATE OPERATOR FAMILY fam USING {am};
             CREATE OPERATOR CLASS oc FOR TYPE int USING {am} FAMILY fam AS {class};"
        );
        let stmt = format!("ALTER OPERATOR FAMILY fam USING {am} {stmt};");
        assert_drop_fails(
            &setup,
            &stmt,
            &format!(
                "cannot drop {member} because operator class oc for access method {am} requires \
                 it (You can drop operator class oc for access method {am} instead.)"
            ),
        );
        // ... and still holds its operator.
        if stmt.contains("OPERATOR 3") || stmt.contains("OPERATOR 1") {
            let err = try_apply(&[
                ("0001.sql", &setup),
                ("0002.sql", "DROP OPERATOR ===(int, int);"),
            ])
            .expect_err("DROP OPERATOR");
            assert!(
                err.to_string()
                    .starts_with("cannot drop operator ===(integer,integer) because other"),
                "{err}"
            );
        }
    }
}

#[test]
fn an_operator_class_holds_on_to_its_required_support_functions() {
    for (am, intype, items, function, proc_args) in [
        (
            "btree",
            "int",
            "OPERATOR 3 =(int, int), FUNCTION 1 mycmp(int, int)",
            "CREATE FUNCTION mycmp(int, int) RETURNS int LANGUAGE sql IMMUTABLE AS 'select btint4cmp($1, $2)';",
            "mycmp(int, int)",
        ),
        (
            "hash",
            "int",
            "OPERATOR 1 =(int, int), FUNCTION 1 mycmp(int)",
            "CREATE FUNCTION mycmp(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'select hashint4($1)';",
            "mycmp(int)",
        ),
    ] {
        let setup = format!(
            "{function}
             CREATE OPERATOR CLASS oc FOR TYPE {intype} USING {am} AS {items};"
        );
        let signature = proc_args.replace("int", "integer").replace(", ", ",");
        assert_drop_fails(
            &setup,
            &format!("DROP FUNCTION {proc_args};"),
            &format!(
                "cannot drop function {signature} because other objects depend on it (operator \
                 class oc for access method {am} depends on function {signature})"
            ),
        );
    }
    // CASCADE takes the class and its indexes.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE FUNCTION mycmp(int, int) RETURNS int LANGUAGE sql IMMUTABLE AS 'select btint4cmp($1, $2)';
             CREATE OPERATOR CLASS oc FOR TYPE int USING btree AS
                 OPERATOR 3 =(int, int), FUNCTION 1 mycmp(int, int);
             CREATE TABLE t (a int);
             CREATE INDEX i ON t (a oc);",
        ),
        ("0002.sql", "DROP FUNCTION mycmp(int, int) CASCADE;"),
        (
            "0003.sql",
            "CREATE OPERATOR CLASS oc FOR TYPE int USING btree AS
                 OPERATOR 3 =(int, int), FUNCTION 1 btint4cmp(int, int);",
        ),
    ]);
    assert!(db.resolve_table(None, "i").is_none());
    // GiST's and GIN's required procs; GIN's comparison proc is optional.
    let gist = "CREATE EXTENSION btree_gist;
                CREATE FUNCTION myunion(internal, internal) RETURNS gbtreekey8 LANGUAGE c STRICT
                    AS '$libdir/btree_gist', 'gbt_int4_union';
                CREATE OPERATOR CLASS my_int4_ops FOR TYPE int4 USING gist AS
                    OPERATOR 3 =,
                    FUNCTION 1 gbt_int4_consistent(internal, int4, int2, oid, internal),
                    FUNCTION 2 myunion(internal, internal),
                    FUNCTION 5 gbt_int4_penalty(internal, internal, internal),
                    FUNCTION 6 gbt_int4_picksplit(internal, internal),
                    FUNCTION 7 gbt_int4_same(gbtreekey8, gbtreekey8, internal),
                    STORAGE gbtreekey8;";
    assert_drop_fails(
        gist,
        "DROP FUNCTION myunion(internal, internal);",
        "cannot drop function myunion(internal,internal) because other objects depend on it \
         (operator class my_int4_ops for access method gist depends on function \
         myunion(internal,internal))",
    );
    let gin = |compare: &str, extract: &str| {
        format!(
            "CREATE EXTENSION btree_gin;
             CREATE FUNCTION mycmp(int4, int4) RETURNS int LANGUAGE sql IMMUTABLE
                 AS 'select btint4cmp($1, $2)';
             CREATE FUNCTION myextract(int4, internal) RETURNS internal LANGUAGE c STRICT IMMUTABLE
                 AS '$libdir/btree_gin', 'gin_extract_value_int4';
             CREATE OPERATOR CLASS my_gin_ops FOR TYPE int4 USING gin AS
                 OPERATOR 3 =,
                 FUNCTION 1 {compare}(int4, int4),
                 FUNCTION 2 {extract}(int4, internal),
                 FUNCTION 3 gin_extract_query_int4(int4, internal, int2, internal, internal),
                 FUNCTION 4 gin_btree_consistent(internal, int2, anyelement, int4, internal, internal);"
        )
    };
    assert_drop_fails(
        &gin("btint4cmp", "myextract"),
        "DROP FUNCTION myextract(int4, internal);",
        "cannot drop function myextract(integer,internal) because other objects depend on it \
         (operator class my_gin_ops for access method gin depends on function \
         myextract(integer,internal))",
    );
    build(&[
        ("0001.sql", &gin("mycmp", "gin_extract_value_int4")),
        ("0002.sql", "DROP FUNCTION mycmp(int4, int4);"),
    ]);
}

#[test]
fn drop_schema_cascade_takes_what_depends_on_its_text_search_objects() {
    build(&[(
        "0001.sql",
        "CREATE SCHEMA s;
         CREATE TEXT SEARCH TEMPLATE s.tm (lexize = dsimple_lexize, init = dsimple_init);
         CREATE TEXT SEARCH DICTIONARY public.d1 (template = s.tm);
         DROP SCHEMA s CASCADE;
         CREATE TEXT SEARCH DICTIONARY public.d1 (template = simple);",
    )]);
}

#[test]
fn every_multi_target_drop_reports_the_objects_together() {
    for (setup, stmt, detail) in [
        (
            "CREATE TEXT SEARCH TEMPLATE t1 (lexize = dsimple_lexize);
             CREATE TEXT SEARCH TEMPLATE t2 (lexize = dsimple_lexize);
             CREATE TEXT SEARCH DICTIONARY d (template = t2);",
            "DROP TEXT SEARCH TEMPLATE t1, t2;",
            " (text search dictionary d depends on text search template t2)",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY a (template = simple);
             CREATE TEXT SEARCH DICTIONARY b (template = simple);
             CREATE TEXT SEARCH CONFIGURATION c (copy = english);
             ALTER TEXT SEARCH CONFIGURATION c ALTER MAPPING FOR asciiword WITH b;",
            "DROP TEXT SEARCH DICTIONARY a, b;",
            " (text search configuration c depends on text search dictionary b)",
        ),
        (
            "CREATE FOREIGN DATA WRAPPER w;
             CREATE SERVER s FOREIGN DATA WRAPPER w;
             CREATE SERVER s2 FOREIGN DATA WRAPPER w;
             CREATE USER MAPPING FOR CURRENT_USER SERVER s;",
            "DROP SERVER s2, s;",
            "",
        ),
        (
            "CREATE FOREIGN DATA WRAPPER w;
             CREATE FOREIGN DATA WRAPPER w2;
             CREATE SERVER s FOREIGN DATA WRAPPER w;",
            "DROP FOREIGN DATA WRAPPER w2, w;",
            "",
        ),
    ] {
        assert_drop_fails(
            setup,
            stmt,
            &format!("cannot drop desired object(s) because other objects depend on them{detail}"),
        );
    }
}

#[test]
fn more_dependency_details_name_the_dependent_as_pg_does() {
    for (setup, stmt, message) in [
        (
            "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select $1';
             CREATE VIEW v AS SELECT f(1);",
            "DROP FUNCTION f(int);",
            "cannot drop function f(integer) because other objects depend on it (view v depends \
             on function f(integer))",
        ),
        (
            "CREATE TABLE t (id int PRIMARY KEY, x int UNIQUE);
             CREATE TABLE c (x int REFERENCES t (x));",
            "ALTER TABLE t DROP COLUMN x;",
            "cannot drop column x of table t because other objects depend on it (constraint \
             c_x_fkey on table c depends on column x of table t)",
        ),
        (
            "CREATE SCHEMA s; CREATE FUNCTION s.f() RETURNS int LANGUAGE sql AS 'select 1';",
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it (function s.f() depends on \
             schema s)",
        ),
        (
            "CREATE SCHEMA s; CREATE TYPE s.e AS ENUM ('a');
             CREATE FUNCTION s.f() RETURNS int LANGUAGE sql AS 'select 1';
             CREATE TABLE s.t (a int);",
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it (type s.e depends on schema \
             s)",
        ),
        (
            "CREATE EXTENSION earthdistance CASCADE;",
            "DROP EXTENSION cube;",
            "cannot drop extension cube because other objects depend on it (extension \
             earthdistance depends on extension cube)",
        ),
    ] {
        assert_drop_fails(setup, stmt, message);
    }
}

const C4: &str =
    "CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
                  CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
                  CREATE FUNCTION c4(int4, int4) RETURNS int LANGUAGE sql IMMUTABLE
                      AS 'select btint4cmp($1, $2)';
                  CREATE OPERATOR CLASS oc FOR TYPE int4 USING btree AS
                      OPERATOR 3 ===, FUNCTION 1 c4(int4, int4);";

#[test]
fn a_renamed_or_moved_family_keeps_its_members() {
    assert_drop_fails(
        &format!("{C4} ALTER OPERATOR FAMILY oc USING btree RENAME TO fam2;"),
        "ALTER OPERATOR FAMILY fam2 USING btree DROP OPERATOR 3 (int4, int4);",
        "cannot drop operator 3 (integer, integer) of operator family fam2 for access method \
         btree: ===(integer,integer) because operator class oc for access method btree requires \
         it",
    );
    // A new family by the old name has no members.
    build(&[
        ("0001.sql", C4),
        (
            "0002.sql",
            "ALTER OPERATOR FAMILY oc USING btree RENAME TO fam2;
             CREATE OPERATOR FAMILY oc USING btree;
             ALTER OPERATOR FAMILY oc USING btree ADD FUNCTION 1 c4(int4, int4);",
        ),
    ]);
    assert_drop_fails(
        &format!("CREATE SCHEMA s; {C4} ALTER OPERATOR FAMILY oc USING btree SET SCHEMA s;"),
        "ALTER OPERATOR FAMILY s.oc USING btree DROP FUNCTION 1 (int4, int4);",
        "cannot drop function 1 (integer, integer) of operator family s.oc for access method \
         btree: c4(integer,integer) because operator class oc for access method btree requires it",
    );
    // Renaming the class leaves its family's name.
    assert_drop_fails(
        &format!("{C4} ALTER OPERATOR CLASS oc USING btree RENAME TO oc2;"),
        "ALTER OPERATOR FAMILY oc USING btree DROP OPERATOR 3 (int4, int4);",
        "cannot drop operator 3 (integer, integer) of operator family oc for access method \
         btree: ===(integer,integer) because operator class oc2 for access method btree requires \
         it",
    );
}

#[test]
fn a_member_is_tied_to_the_first_class_by_name_then_schema() {
    assert_drop_fails(
        "CREATE SCHEMA s;
         CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
         CREATE OPERATOR FAMILY fam USING btree;
         CREATE OPERATOR CLASS z_oc FOR TYPE int4 USING btree FAMILY fam AS STORAGE int4;
         CREATE OPERATOR CLASS s.a_oc FOR TYPE int4 USING btree FAMILY fam AS STORAGE int4;
         ALTER OPERATOR FAMILY fam USING btree ADD OPERATOR 3 ===(int4, int4);",
        "DROP OPERATOR ===(int4, int4);",
        "cannot drop operator ===(integer,integer) because other objects depend on it (operator \
         class s.a_oc for access method btree depends on operator ===(integer,integer))",
    );
}

#[test]
fn create_operator_class_refuses_a_support_function_the_family_has() {
    assert_drop_fails(
        "CREATE FUNCTION c4(int4, int4) RETURNS int LANGUAGE sql IMMUTABLE
             AS 'select btint4cmp($1, $2)';
         CREATE OPERATOR FAMILY fam USING btree;
         ALTER OPERATOR FAMILY fam USING btree ADD FUNCTION 1 c4(int4, int4);",
        "CREATE OPERATOR CLASS oc FOR TYPE int4 USING btree FAMILY fam AS FUNCTION 1 c4(int4, int4);",
        "duplicate key value violates unique constraint \"pg_amproc_fam_proc_index\"",
    );
}

#[test]
fn support_functions_are_checked_as_assign_proc_types_does() {
    let class = |am: &str, items: &str| {
        format!("CREATE OPERATOR CLASS oc FOR TYPE int4 USING {am} AS {items};")
    };
    for (setup, stmt, message) in [
        (
            "",
            class("btree", "FUNCTION 7 btint4cmp(int4, int4)"),
            "invalid function number 7, must be between 1 and 6",
        ),
        (
            "",
            class("hash", "FUNCTION 4 hashint4(int4)"),
            "invalid function number 4, must be between 1 and 3",
        ),
        (
            "",
            class("gist", "FUNCTION 13 btint4cmp(int4, int4)"),
            "invalid function number 13, must be between 1 and 12",
        ),
        (
            "",
            class("btree", "FUNCTION 0 nosuch(int4)"),
            "invalid function number 0, must be between 1 and 6",
        ),
        (
            "CREATE EXTENSION bloom;",
            class("bloom", "FUNCTION 3 hashint4(int4)"),
            "invalid function number 3, must be between 1 and 2",
        ),
        (
            "",
            class("brin", "FUNCTION 16 hashint4(int4)"),
            "invalid function number 16, must be between 1 and 15",
        ),
        (
            "",
            class("btree", "FUNCTION 2 btint4cmp(int4, int4)"),
            "ordering sort support functions must accept type \"internal\"",
        ),
        (
            "CREATE FUNCTION ss(internal) RETURNS int LANGUAGE c AS '$libdir/plpgsql', 'plpgsql_call_handler';",
            class("btree", "FUNCTION 2 ss(internal)"),
            "ordering sort support functions must return void",
        ),
        (
            "",
            class("btree", "FUNCTION 6 btint4cmp(int4, int4)"),
            "btree skip support functions must accept type \"internal\"",
        ),
        (
            "",
            class("btree", "FUNCTION 3 btint4cmp(int4, int4)"),
            "ordering in_range functions must have five arguments",
        ),
        (
            "",
            class("btree", "FUNCTION 1 hashint4(int4)"),
            "ordering comparison functions must have two arguments",
        ),
        (
            "",
            class("btree", "FUNCTION 1 int4eq(int4, int4)"),
            "ordering comparison functions must return integer",
        ),
        (
            "",
            class("btree", "FUNCTION 4 (int4, int8) btequalimage(oid)"),
            "ordering equal image functions must not be cross-type",
        ),
        (
            "",
            class("btree", "FUNCTION 5 btint4cmp(int4, int4)"),
            "invalid operator class options parsing function",
        ),
        (
            "",
            class("hash", "FUNCTION 3 hashint4(int4)"),
            "invalid operator class options parsing function",
        ),
        (
            "",
            class("gist", "FUNCTION 10 btint4cmp(int4, int4)"),
            "invalid operator class options parsing function",
        ),
        (
            "",
            class("brin", "FUNCTION 5 hashint4(int4)"),
            "invalid operator class options parsing function",
        ),
        (
            "",
            class("hash", "FUNCTION 1 btint4cmp(int4, int4)"),
            "hash function 1 must have one argument",
        ),
        (
            "",
            class("hash", "FUNCTION 2 hashint4(int4)"),
            "hash function 2 must have two arguments",
        ),
        (
            "CREATE OPERATOR FAMILY fam USING gist;",
            "ALTER OPERATOR FAMILY fam USING gist ADD FUNCTION 1 btint4cmp(int4, int4);".to_owned(),
            "associated data types must be specified for index support function",
        ),
        (
            "CREATE OPERATOR FAMILY fam USING gist;",
            "ALTER OPERATOR FAMILY fam USING gist ADD FUNCTION 13 btint4cmp(int4, int4);"
                .to_owned(),
            "invalid function number 13, must be between 1 and 12",
        ),
    ] {
        let setup = if setup.is_empty() { "SELECT 1;" } else { setup };
        assert_drop_fails(setup, &stmt, message);
    }
    build(&[(
        "0001.sql",
        "CREATE OPERATOR CLASS oc FOR TYPE int4 USING btree AS FUNCTION 4 btequalimage(oid);",
    )]);
}

// ── Text search configurations, operator classes' types, misc ───────────────

#[test]
fn a_configuration_depends_on_what_its_mapping_names_now() {
    let dicts = "CREATE TEXT SEARCH DICTIONARY d1 (template = simple);
                 CREATE TEXT SEARCH DICTIONARY d2 (template = simple);
                 CREATE TEXT SEARCH CONFIGURATION c (copy = english);
                 ALTER TEXT SEARCH CONFIGURATION c ALTER MAPPING FOR asciiword WITH d1;";
    // Mapping elsewhere releases d1.
    for change in [
        "ALTER TEXT SEARCH CONFIGURATION c ALTER MAPPING FOR asciiword WITH d2;",
        "ALTER TEXT SEARCH CONFIGURATION c ALTER MAPPING REPLACE d1 WITH d2;",
        "ALTER TEXT SEARCH CONFIGURATION c ALTER MAPPING FOR asciiword REPLACE d1 WITH d2;",
        "ALTER TEXT SEARCH CONFIGURATION c DROP MAPPING FOR asciiword;",
    ] {
        build(&[
            ("0001.sql", dicts),
            ("0002.sql", change),
            ("0003.sql", "DROP TEXT SEARCH DICTIONARY d1;"),
        ]);
    }
    // Still mapped for another token type.
    assert_drop_fails(
        "CREATE TEXT SEARCH DICTIONARY d1 (template = simple);
         CREATE TEXT SEARCH CONFIGURATION c (copy = english);
         ALTER TEXT SEARCH CONFIGURATION c ALTER MAPPING FOR asciiword, email WITH d1;
         ALTER TEXT SEARCH CONFIGURATION c DROP MAPPING FOR asciiword;",
        "DROP TEXT SEARCH DICTIONARY d1;",
        "cannot drop text search dictionary d1 because other objects depend on it (text search \
         configuration c depends on text search dictionary d1)",
    );
    // A copy depends on the dictionaries it copied.
    assert_drop_fails(
        "CREATE TEXT SEARCH DICTIONARY d1 (template = simple);
         CREATE TEXT SEARCH CONFIGURATION c (parser = default);
         ALTER TEXT SEARCH CONFIGURATION c ADD MAPPING FOR email WITH d1;
         CREATE TEXT SEARCH CONFIGURATION c2 (copy = c);
         DROP TEXT SEARCH CONFIGURATION c;",
        "DROP TEXT SEARCH DICTIONARY d1;",
        "cannot drop text search dictionary d1 because other objects depend on it (text search \
         configuration c2 depends on text search dictionary d1)",
    );
}

#[test]
fn a_configuration_mapping_names_each_token_type_once() {
    for (setup, stmt, message) in [
        (
            "CREATE TEXT SEARCH CONFIGURATION c (copy = english);",
            "ALTER TEXT SEARCH CONFIGURATION c ADD MAPPING FOR email WITH simple;",
            "duplicate key value violates unique constraint \"pg_ts_config_map_index\"",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION c (parser = default);
             ALTER TEXT SEARCH CONFIGURATION c ADD MAPPING FOR email WITH simple;",
            "ALTER TEXT SEARCH CONFIGURATION c ADD MAPPING FOR email WITH simple;",
            "duplicate key value violates unique constraint \"pg_ts_config_map_index\"",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION c (parser = default);",
            "ALTER TEXT SEARCH CONFIGURATION c DROP MAPPING FOR email;",
            "mapping for token type \"email\" does not exist",
        ),
    ] {
        assert_drop_fails(setup, stmt, message);
    }
    build(&[(
        "0001.sql",
        "CREATE TEXT SEARCH CONFIGURATION c (parser = default);
         ALTER TEXT SEARCH CONFIGURATION c ALTER MAPPING FOR email WITH simple;
         ALTER TEXT SEARCH CONFIGURATION c DROP MAPPING IF EXISTS FOR url;",
    )]);
}

#[test]
fn a_configuration_depends_on_its_parser() {
    let parser = "CREATE TEXT SEARCH PARSER pp (start = prsd_start, gettoken = prsd_nexttoken,
                      end = prsd_end, lextypes = prsd_lextype);
                  CREATE TEXT SEARCH CONFIGURATION c (parser = pp);
                  CREATE TEXT SEARCH CONFIGURATION c2 (copy = c);";
    assert_drop_fails(
        parser,
        "DROP TEXT SEARCH PARSER pp;",
        "cannot drop text search parser pp because other objects depend on it (text search \
         configuration c depends on text search parser pp)",
    );
    build(&[
        ("0001.sql", parser),
        ("0002.sql", "DROP TEXT SEARCH PARSER pp CASCADE;"),
        ("0003.sql", parser),
    ]);
}

#[test]
fn an_operator_class_depends_on_its_input_type() {
    let class = "CREATE TYPE ty AS (a int);
                 CREATE OPERATOR CLASS oc FOR TYPE ty USING hash AS FUNCTION 1 hashint4(int);";
    assert_drop_fails(
        class,
        "DROP TYPE ty;",
        "cannot drop type ty because other objects depend on it (operator class oc for access \
         method hash depends on type ty)",
    );
    // CASCADE takes the class, not its family — nor the family's loose
    // hashint4 member (int is not the class's type).
    build(&[
        ("0001.sql", class),
        ("0002.sql", "DROP TYPE ty CASCADE;"),
        (
            "0003.sql",
            "CREATE TYPE ty AS (a int);
             CREATE OPERATOR CLASS oc FOR TYPE ty USING hash FAMILY oc AS OPERATOR 1 =(int, int);",
        ),
    ]);
    assert_drop_fails(
        &format!("{class} DROP TYPE ty CASCADE; CREATE TYPE ty AS (a int);"),
        "CREATE OPERATOR CLASS oc FOR TYPE ty USING hash FAMILY oc AS FUNCTION 1 hashint4(int);",
        "duplicate key value violates unique constraint \"pg_amproc_fam_proc_index\"",
    );
    assert_drop_fails(
        "CREATE EXTENSION cube;
         CREATE OPERATOR CLASS my_cube_ops FOR TYPE cube USING gist AS
             OPERATOR 3 &&,
             FUNCTION 1 g_cube_consistent(internal, cube, smallint, oid, internal),
             FUNCTION 2 g_cube_union(internal, internal),
             FUNCTION 5 g_cube_penalty(internal, internal, internal),
             FUNCTION 6 g_cube_picksplit(internal, internal),
             FUNCTION 7 g_cube_same(cube, cube, internal);",
        "DROP EXTENSION cube;",
        "cannot drop extension cube because other objects depend on it",
    );
}

#[test]
fn drop_routine_checks_what_depends_on_the_routine() {
    assert_drop_fails(
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select $1';
         CREATE VIEW v AS SELECT f(1);",
        "DROP ROUTINE f(int);",
        "cannot drop function f(integer) because other objects depend on it (view v depends on \
         function f(integer))",
    );
    let db = build_db(&[(
        "0001.sql",
        "CREATE PROCEDURE p(int) LANGUAGE sql AS 'select 1';
         CREATE FUNCTION g(int) RETURNS int LANGUAGE sql AS 'select $1';
         DROP ROUTINE p(int), g(int);",
    )]);
    assert!(db.find_functions(None, "p").is_empty());
    assert!(db.find_functions(None, "g").is_empty());
}

#[test]
fn operator_class_items_are_checked_as_pg_does() {
    for (setup, stmt, message) in [
        (
            "",
            "CREATE OPERATOR CLASS oc FOR TYPE int USING btree AS
                 OPERATOR 1 <<<<(int, int), FUNCTION 1 btint4cmp(int, int);",
            "operator does not exist: integer <<<< integer",
        ),
        (
            "CREATE OPERATOR FAMILY fam USING btree;",
            "ALTER OPERATOR FAMILY fam USING btree ADD OPERATOR 1 <<<<(int, int);",
            "operator does not exist: integer <<<< integer",
        ),
        (
            "",
            "CREATE OPERATOR CLASS oc FOR TYPE int USING hash AS
                 OPERATOR 3 =(int, int), FUNCTION 1 hashint4(int);",
            "invalid operator number 3, must be between 1 and 1",
        ),
        (
            "CREATE EXTENSION bloom;",
            "CREATE OPERATOR CLASS oc FOR TYPE int USING bloom AS
                 OPERATOR 3 =(int, int), FUNCTION 1 hashint4(int);",
            "invalid operator number 3, must be between 1 and 1",
        ),
        (
            "CREATE OPERATOR FAMILY fam USING hash;",
            "ALTER OPERATOR FAMILY fam USING hash ADD OPERATOR 2 =(int, int);",
            "invalid operator number 2, must be between 1 and 1",
        ),
        (
            "",
            "CREATE OPERATOR CLASS oc FOR TYPE int USING btree AS
                 OPERATOR 6 =(int, int), FUNCTION 1 btint4cmp(int, int);",
            "invalid operator number 6, must be between 1 and 5",
        ),
    ] {
        let setup = if setup.is_empty() { "SELECT 1;" } else { setup };
        assert_drop_fails(setup, stmt, message);
    }
    // GiST takes any strategy number.
    build(&[(
        "0001.sql",
        "CREATE OPERATOR CLASS oc FOR TYPE int USING gist AS OPERATOR 100 =(int, int);",
    )]);
}

#[test]
fn an_operator_moved_to_another_schema_is_found_there() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA s2;
         CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
         ALTER OPERATOR ===(int, int) SET SCHEMA s2;",
    )]);
    db.analyze("SELECT 1 OPERATOR(s2.===) 1").unwrap();
    assert!(db.analyze("SELECT 1 === 1").is_err());
    build(&[(
        "0001.sql",
        "CREATE SCHEMA s2;
         CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
         ALTER OPERATOR ===(int, int) SET SCHEMA s2;
         DROP OPERATOR s2.===(int, int);",
    )]);
    assert_drop_fails(
        "CREATE SCHEMA s2;
         CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
         CREATE OPERATOR s2.=== (leftarg = int, rightarg = int, function = f);",
        "ALTER OPERATOR ===(int, int) SET SCHEMA s2;",
        "duplicate key value violates unique constraint \"pg_operator_oprname_l_r_n_index\"",
    );
}

#[test]
fn alter_extension_adds_and_drops_operators() {
    // Out of the extension, the operator drops on its own — and then
    // holds on to the extension's type.
    build(&[(
        "0001.sql",
        "CREATE EXTENSION hstore;
         ALTER EXTENSION hstore DROP OPERATOR ->(hstore, text);
         DROP OPERATOR ->(hstore, text);",
    )]);
    assert_drop_fails(
        "CREATE EXTENSION hstore; ALTER EXTENSION hstore DROP OPERATOR ->(hstore, text);",
        "DROP EXTENSION hstore;",
        "cannot drop extension hstore because other objects depend on it",
    );
    assert_drop_fails(
        "CREATE EXTENSION hstore; ALTER EXTENSION hstore DROP OPERATOR ->(hstore, text);",
        "ALTER EXTENSION hstore DROP OPERATOR ->(hstore, text);",
        "operator ->(hstore,text) is not a member of extension \"hstore\"",
    );
    assert_drop_fails(
        "CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = f);
         CREATE EXTENSION hstore;
         ALTER EXTENSION hstore ADD OPERATOR ===(int, int);",
        "DROP OPERATOR ===(int, int);",
        "cannot drop operator ===(integer,integer) because extension hstore requires it",
    );
}

#[test]
fn drop_schema_names_its_oldest_object_of_any_kind() {
    for (objects, oldest) in [
        (
            "CREATE TEXT SEARCH TEMPLATE s.tm (lexize = dsimple_lexize);
             CREATE FUNCTION s.f() RETURNS int LANGUAGE sql AS 'select 1';",
            "text search template s.tm",
        ),
        (
            "CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
             CREATE OPERATOR s.=== (leftarg = int, rightarg = int, function = f);
             CREATE TABLE s.t (a int);",
            "operator s.===(integer,integer)",
        ),
        (
            "CREATE OPERATOR FAMILY s.fam USING btree; CREATE TABLE s.t (a int);",
            "operator family s.fam for access method btree",
        ),
        (
            "CREATE COLLATION s.c (locale = 'C'); CREATE TABLE s.t (a int);",
            "collation s.c",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY s.d (template = simple); CREATE TABLE s.t (a int);",
            "text search dictionary s.d",
        ),
    ] {
        assert_drop_fails(
            &format!("CREATE SCHEMA s; {objects}"),
            "DROP SCHEMA s;",
            &format!(
                "cannot drop schema s because other objects depend on it ({oldest} depends on \
                 schema s)"
            ),
        );
    }
}

#[test]
fn drop_operator_needs_its_operand_types_to_exist() {
    for stmt in [
        "DROP OPERATOR @>(nosuchtype, int4);",
        "DROP OPERATOR @>(int4, nosuchtype);",
    ] {
        assert_drop_fails("SELECT 1;", stmt, "type \"nosuchtype\" does not exist");
    }
    build(&[(
        "0001.sql",
        "DROP OPERATOR IF EXISTS @>(nosuchtype, int4);
         DROP OPERATOR IF EXISTS @>(int4, nosuchtype);",
    )]);
}

#[test]
fn a_configuration_mapping_counts_a_repeated_token_type_once() {
    let mapped = "CREATE TEXT SEARCH CONFIGURATION c2 (copy = english);
                  ALTER TEXT SEARCH CONFIGURATION c2 DROP MAPPING FOR email;
                  ALTER TEXT SEARCH CONFIGURATION c2 ADD MAPPING FOR email, email WITH simple;";
    build(&[
        ("0001.sql", mapped),
        (
            "0002.sql",
            "ALTER TEXT SEARCH CONFIGURATION c2 DROP MAPPING FOR email, email;",
        ),
    ]);
    assert_drop_fails(
        &format!("{mapped} ALTER TEXT SEARCH CONFIGURATION c2 DROP MAPPING FOR email, email;"),
        "ALTER TEXT SEARCH CONFIGURATION c2 DROP MAPPING FOR email;",
        "mapping for token type \"email\" does not exist",
    );
    // A token type mapped already still trips the unique index.
    assert_drop_fails(
        "CREATE TEXT SEARCH CONFIGURATION c2 (copy = english);
         ALTER TEXT SEARCH CONFIGURATION c2 DROP MAPPING FOR email, url;
         ALTER TEXT SEARCH CONFIGURATION c2 ADD MAPPING FOR email, url, email WITH simple;",
        "ALTER TEXT SEARCH CONFIGURATION c2 ADD MAPPING FOR url WITH simple;",
        "duplicate key value violates unique constraint \"pg_ts_config_map_index\"",
    );
}

#[test]
fn an_operator_class_depends_on_its_storage_type() {
    let class = "CREATE TYPE t2 AS (a int);
                 CREATE OPERATOR CLASS oc FOR TYPE int4 USING gist AS STORAGE t2;";
    assert_drop_fails(
        class,
        "DROP TYPE t2;",
        "cannot drop type t2 because other objects depend on it (operator class oc for access \
         method gist depends on type t2)",
    );
    // An array storage type is part of its element type.
    assert_drop_fails(
        "CREATE TYPE t2 AS (a int);
         CREATE OPERATOR CLASS oc FOR TYPE int4 USING gist AS STORAGE t2[];",
        "DROP TYPE t2;",
        "cannot drop type t2 because other objects depend on it",
    );
    // CASCADE takes the class, not its family.
    build(&[
        ("0001.sql", class),
        ("0002.sql", "DROP TYPE t2 CASCADE;"),
        (
            "0003.sql",
            "CREATE OPERATOR CLASS oc FOR TYPE int4 USING gist FAMILY oc AS STORAGE int4;",
        ),
    ]);
    // A storage type the input type's is no storage type.
    build(&[(
        "0001.sql",
        "CREATE OPERATOR CLASS oc FOR TYPE int4 USING gist AS STORAGE int4;",
    )]);
}

#[test]
fn an_operator_class_storage_type_is_checked_as_pg_does() {
    for (setup, stmt, message) in [
        (
            "CREATE TYPE t2 AS (a int);",
            "CREATE OPERATOR CLASS oc FOR TYPE int4 USING hash AS
                 STORAGE t2, FUNCTION 1 hashint4(int);",
            "storage type cannot be different from data type for access method \"hash\"",
        ),
        (
            "CREATE TYPE t2 AS (a int);",
            "CREATE OPERATOR CLASS oc FOR TYPE int4 USING btree AS STORAGE t2, OPERATOR 1 <;",
            "storage type cannot be different from data type for access method \"btree\"",
        ),
        (
            "CREATE TYPE t2 AS (a int); CREATE EXTENSION bloom;",
            "CREATE OPERATOR CLASS oc FOR TYPE int4 USING bloom AS
                 STORAGE t2, FUNCTION 1 hashint4(int);",
            "storage type cannot be different from data type for access method \"bloom\"",
        ),
        (
            "CREATE TYPE t2 AS (a int);",
            "CREATE OPERATOR CLASS oc FOR TYPE int4 USING gist AS STORAGE t2, STORAGE t2;",
            "storage type specified more than once",
        ),
        (
            "SELECT 1;",
            "CREATE OPERATOR CLASS oc FOR TYPE int4 USING gist AS STORAGE nope;",
            "type \"nope\" does not exist",
        ),
    ] {
        assert_drop_fails(setup, stmt, message);
    }
    build(&[(
        "0001.sql",
        "CREATE TYPE t2 AS (a int);
         CREATE OPERATOR CLASS a FOR TYPE int4 USING spgist AS STORAGE t2;
         CREATE OPERATOR CLASS b FOR TYPE int4 USING gin AS STORAGE t2;
         CREATE OPERATOR CLASS c FOR TYPE int4 USING brin AS STORAGE t2;
         CREATE OPERATOR CLASS d FOR TYPE int4 USING btree AS STORAGE int4, OPERATOR 1 <;",
    )]);
}

#[test]
fn a_missing_operator_is_reported_as_pg_does() {
    let setup = "CREATE SCHEMA s; CREATE SCHEMA \"My S\";";
    for (stmt, message) in [
        // op_signature_string: no left side for a prefix operator, the
        // name as written.
        (
            "DROP OPERATOR === (NONE, int);",
            "operator does not exist: === integer",
        ),
        (
            "ALTER OPERATOR === (NONE, int) SET SCHEMA s;",
            "operator does not exist: === integer",
        ),
        (
            "DROP OPERATOR s.=== (int, int);",
            "operator does not exist: integer s.=== integer",
        ),
        (
            "ALTER OPERATOR s.=== (int, int) SET SCHEMA public;",
            "operator does not exist: integer s.=== integer",
        ),
        (
            "DROP OPERATOR s.=== (NONE, int);",
            "operator does not exist: s.=== integer",
        ),
        (
            "DROP OPERATOR \"My S\".=== (int, int);",
            "operator does not exist: integer My S.=== integer",
        ),
        (
            "DROP OPERATOR nosuch.=== (int, int);",
            "operator does not exist: integer nosuch.=== integer",
        ),
        (
            "DROP OPERATOR === (int[], text);",
            "operator does not exist: integer[] === text",
        ),
        (
            "ALTER OPERATOR === (NONE, int) SET (restrict = eqsel);",
            "operator does not exist: === integer",
        ),
        (
            "ALTER OPERATOR s.=== (int, int) SET (restrict = eqsel);",
            "operator does not exist: integer s.=== integer",
        ),
        (
            "COMMENT ON OPERATOR === (NONE, int) IS 'x';",
            "operator does not exist: === integer",
        ),
        (
            "ALTER OPERATOR s.=== (int, int) OWNER TO CURRENT_USER;",
            "operator does not exist: integer s.=== integer",
        ),
        (
            "CREATE FUNCTION neg(int) RETURNS int LANGUAGE sql AS 'select -$1';
             CREATE OPERATOR ~~~ (rightarg = int, function = neg);
             ALTER OPERATOR ~~~ (NONE, int) SET (negator = !!!);",
            "operator does not exist: !!! integer",
        ),
        // LookupOperName: no postfix operator exists any more.
        (
            "DROP OPERATOR === (int, NONE);",
            "postfix operators are not supported",
        ),
        (
            "ALTER OPERATOR === (int, NONE) SET SCHEMA s;",
            "postfix operators are not supported",
        ),
        (
            "ALTER OPERATOR = (int, NONE) SET (restrict = eqsel);",
            "postfix operators are not supported",
        ),
        (
            "COMMENT ON OPERATOR = (int, NONE) IS 'x';",
            "postfix operators are not supported",
        ),
        (
            "ALTER OPERATOR = (int, NONE) OWNER TO CURRENT_USER;",
            "postfix operators are not supported",
        ),
        (
            "ALTER EXTENSION plpgsql ADD OPERATOR = (int, NONE);",
            "postfix operators are not supported",
        ),
        (
            "DROP OPERATOR === (nosuch, NONE);",
            "type \"nosuch\" does not exist",
        ),
    ] {
        assert_drop_fails(setup, stmt, message);
    }
    build(&[(
        "0001.sql",
        "DROP OPERATOR IF EXISTS === (int, NONE);
         DROP OPERATOR IF EXISTS === (nosuch, NONE);
         DROP OPERATOR IF EXISTS nosuch.=== (int, int);",
    )]);
}

#[test]
fn an_operator_family_member_is_looked_up_as_pg_does() {
    for (stmt, message) in [
        (
            "ALTER OPERATOR FAMILY f USING btree ADD OPERATOR 1 - (NONE, int);",
            "index operators must be binary",
        ),
        (
            "ALTER OPERATOR FAMILY f USING btree ADD OPERATOR 1 === (NONE, int);",
            "operator does not exist: === integer",
        ),
        (
            "ALTER OPERATOR FAMILY f USING btree ADD OPERATOR 1 < (int, NONE);",
            "postfix operators are not supported",
        ),
        (
            "ALTER OPERATOR FAMILY f USING btree ADD OPERATOR 1 === (nosuch, NONE);",
            "type \"nosuch\" does not exist",
        ),
        (
            "ALTER OPERATOR FAMILY f USING btree ADD OPERATOR 1 < (int, nosuch);",
            "type \"nosuch\" does not exist",
        ),
        (
            "CREATE OPERATOR CLASS oc FOR TYPE int USING btree AS OPERATOR 1 - (NONE, int);",
            "index operators must be binary",
        ),
        (
            "CREATE OPERATOR CLASS oc FOR TYPE int USING btree AS OPERATOR 1 s.=== (int, int);",
            "operator does not exist: integer s.=== integer",
        ),
    ] {
        assert_drop_fails(
            "CREATE SCHEMA s; CREATE OPERATOR FAMILY f USING btree;",
            stmt,
            message,
        );
    }
}

#[test]
fn set_schema_never_moves_into_or_out_of_the_temporary_or_toast_schema() {
    let objects = "CREATE SCHEMA s;
         CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE PROCEDURE p(int) LANGUAGE sql AS 'select 1';
         CREATE AGGREGATE ag(int) (sfunc = int4pl, stype = int);
         CREATE TYPE ty AS (a int);
         CREATE DOMAIN dom AS int;
         CREATE TYPE en AS ENUM ('a');
         CREATE OPERATOR === (leftarg = int, rightarg = int, function = int4eq);
         CREATE OPERATOR FAMILY fam USING btree;
         CREATE OPERATOR CLASS oc FOR TYPE int USING hash AS FUNCTION 1 hashint4(int);
         CREATE COLLATION co (locale = 'C');
         CREATE CONVERSION cv FOR 'UTF8' TO 'LATIN1' FROM utf8_to_iso8859_1;
         CREATE TABLE t (a int, b int);
         CREATE STATISTICS st ON a, b FROM t;
         CREATE TEXT SEARCH CONFIGURATION tc (copy = english);
         CREATE TEXT SEARCH DICTIONARY td (template = simple);
         CREATE TEXT SEARCH PARSER tp (start = prsd_start, gettoken = prsd_nexttoken,
             end = prsd_end, lextypes = prsd_lextype);
         CREATE TEXT SEARCH TEMPLATE tt (lexize = dsimple_lexize);
         CREATE EXTENSION citext SCHEMA s;";
    let temporary = "cannot move objects into or out of temporary schemas";
    let toast = "cannot move objects into or out of TOAST schema";
    for (stmt, message) in [
        ("ALTER FUNCTION f(int) SET SCHEMA pg_temp;", temporary),
        ("ALTER PROCEDURE p(int) SET SCHEMA pg_temp;", temporary),
        ("ALTER ROUTINE f(int) SET SCHEMA pg_temp;", temporary),
        ("ALTER AGGREGATE ag(int) SET SCHEMA pg_temp;", temporary),
        ("ALTER TYPE ty SET SCHEMA pg_temp;", temporary),
        ("ALTER TYPE en SET SCHEMA pg_temp;", temporary),
        ("ALTER DOMAIN dom SET SCHEMA pg_temp;", temporary),
        (
            "ALTER OPERATOR === (int, int) SET SCHEMA pg_temp;",
            temporary,
        ),
        (
            "ALTER OPERATOR FAMILY fam USING btree SET SCHEMA pg_temp;",
            temporary,
        ),
        (
            "ALTER OPERATOR CLASS oc USING hash SET SCHEMA pg_temp;",
            temporary,
        ),
        ("ALTER COLLATION co SET SCHEMA pg_temp;", temporary),
        ("ALTER CONVERSION cv SET SCHEMA pg_temp;", temporary),
        ("ALTER STATISTICS st SET SCHEMA pg_temp;", temporary),
        (
            "ALTER TEXT SEARCH CONFIGURATION tc SET SCHEMA pg_temp;",
            temporary,
        ),
        (
            "ALTER TEXT SEARCH DICTIONARY td SET SCHEMA pg_temp;",
            temporary,
        ),
        ("ALTER TEXT SEARCH PARSER tp SET SCHEMA pg_temp;", temporary),
        (
            "ALTER TEXT SEARCH TEMPLATE tt SET SCHEMA pg_temp;",
            temporary,
        ),
        ("ALTER EXTENSION citext SET SCHEMA pg_temp;", temporary),
        ("ALTER FUNCTION f(int) SET SCHEMA pg_toast;", toast),
        ("ALTER TYPE ty SET SCHEMA pg_toast;", toast),
        ("ALTER CONVERSION cv SET SCHEMA pg_toast;", toast),
        ("ALTER STATISTICS st SET SCHEMA pg_toast;", toast),
        (
            "ALTER OPERATOR FAMILY fam USING btree SET SCHEMA pg_toast;",
            toast,
        ),
        ("ALTER EXTENSION citext SET SCHEMA pg_toast;", toast),
        // The object is looked up first.
        (
            "ALTER FUNCTION nof(int) SET SCHEMA pg_temp;",
            "function nof(integer) does not exist",
        ),
        (
            "ALTER TYPE noty SET SCHEMA pg_temp;",
            "type \"noty\" does not exist",
        ),
        (
            "ALTER EXTENSION noext SET SCHEMA nosch;",
            "extension \"noext\" does not exist",
        ),
        (
            "ALTER TYPE ty SET SCHEMA pg_temp_3;",
            "schema \"pg_temp_3\" does not exist",
        ),
        // The control file is checked before the schema.
        (
            "ALTER EXTENSION plpgsql SET SCHEMA pg_temp;",
            "extension \"plpgsql\" does not support SET SCHEMA",
        ),
        // Out of it, too.
        (
            "CREATE FUNCTION pg_temp.g(int) RETURNS int LANGUAGE sql AS 'select 1';
             ALTER FUNCTION pg_temp.g(int) SET SCHEMA public;",
            temporary,
        ),
        (
            "CREATE TYPE pg_temp.ty2 AS (a int); ALTER TYPE pg_temp.ty2 SET SCHEMA public;",
            temporary,
        ),
        (
            "CREATE OPERATOR pg_temp.=== (leftarg = int, rightarg = int, function = int4eq);
             ALTER OPERATOR pg_temp.=== (int, int) SET SCHEMA public;",
            temporary,
        ),
        (
            "CREATE OPERATOR FAMILY pg_temp.fam2 USING btree;
             ALTER OPERATOR FAMILY pg_temp.fam2 USING btree SET SCHEMA public;",
            temporary,
        ),
        (
            "CREATE CONVERSION pg_temp.cv2 FOR 'UTF8' TO 'LATIN1' FROM utf8_to_iso8859_1;
             ALTER CONVERSION pg_temp.cv2 SET SCHEMA public;",
            temporary,
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY pg_temp.td2 (template = simple);
             ALTER TEXT SEARCH DICTIONARY pg_temp.td2 SET SCHEMA public;",
            temporary,
        ),
    ] {
        assert_drop_fails(objects, stmt, message);
    }
    // Where it is already, nothing moves.
    build(&[
        ("0001.sql", objects),
        (
            "0002.sql",
            "ALTER EXTENSION plpgsql SET SCHEMA pg_catalog;
             CREATE FUNCTION pg_temp.g(int) RETURNS int LANGUAGE sql AS 'select 1';
             ALTER FUNCTION pg_temp.g(int) SET SCHEMA pg_temp;",
        ),
    ]);
}

#[test]
fn conversions_and_statistics_objects_move_to_another_schema() {
    let objects = "CREATE SCHEMA s; CREATE SCHEMA s2;
                   CREATE TABLE t (a int, b int);
                   CREATE CONVERSION s.cv FOR 'UTF8' TO 'LATIN1' FROM utf8_to_iso8859_1;
                   CREATE STATISTICS s.st ON a, b FROM t;";
    build(&[
        ("0001.sql", objects),
        (
            "0002.sql",
            "ALTER CONVERSION s.cv SET SCHEMA s2;
             ALTER STATISTICS s.st SET SCHEMA s2;
             DROP SCHEMA s;
             DROP CONVERSION s2.cv;
             DROP STATISTICS s2.st;",
        ),
    ]);
    for (stmt, message) in [
        (
            "CREATE CONVERSION s2.cv FOR 'UTF8' TO 'LATIN1' FROM utf8_to_iso8859_1;
             ALTER CONVERSION s.cv SET SCHEMA s2;",
            "conversion \"cv\" already exists in schema \"s2\"",
        ),
        (
            "CREATE STATISTICS s2.st ON a, b FROM t; ALTER STATISTICS s.st SET SCHEMA s2;",
            "statistics object \"st\" already exists in schema \"s2\"",
        ),
        (
            "ALTER CONVERSION nocv SET SCHEMA s2;",
            "conversion \"nocv\" does not exist",
        ),
        (
            "ALTER STATISTICS nost SET SCHEMA s2;",
            "statistics object \"nost\" does not exist",
        ),
        // get_conversion_oid, get_statistics_object_oid: the schema first.
        (
            "DROP CONVERSION nosch.cv;",
            "schema \"nosch\" does not exist",
        ),
        (
            "DROP STATISTICS nosch.st;",
            "schema \"nosch\" does not exist",
        ),
        (
            "ALTER CONVERSION nosch.cv RENAME TO x;",
            "schema \"nosch\" does not exist",
        ),
        (
            "ALTER STATISTICS nosch.st RENAME TO x;",
            "schema \"nosch\" does not exist",
        ),
        (
            "ALTER STATISTICS nosch.st SET STATISTICS 1;",
            "schema \"nosch\" does not exist",
        ),
        (
            "ALTER CONVERSION nosch.cv SET SCHEMA public;",
            "schema \"nosch\" does not exist",
        ),
        (
            "COMMENT ON CONVERSION nosch.cv IS 'x';",
            "schema \"nosch\" does not exist",
        ),
        (
            "COMMENT ON STATISTICS nosch.st IS 'x';",
            "schema \"nosch\" does not exist",
        ),
        (
            "DROP SCHEMA s CASCADE; DROP CONVERSION s.cv;",
            "schema \"s\" does not exist",
        ),
    ] {
        assert_drop_fails(objects, stmt, message);
    }
    build(&[(
        "0001.sql",
        "DROP CONVERSION IF EXISTS nosch.cv;
         DROP STATISTICS IF EXISTS nosch.st;
         ALTER STATISTICS IF EXISTS nosch.st SET STATISTICS 1;",
    )]);
}

#[test]
fn drop_schema_names_conversions_statistics_and_extensions() {
    for (setup, schema, oldest) in [
        (
            "CREATE SCHEMA s;
             CREATE CONVERSION s.cv FOR 'UTF8' TO 'LATIN1' FROM utf8_to_iso8859_1;
             CREATE TABLE s.t (a int, b int);",
            "s",
            "conversion s.cv",
        ),
        (
            "CREATE SCHEMA s;
             CREATE CONVERSION s.cv FOR 'UTF8' TO 'LATIN1' FROM utf8_to_iso8859_1;",
            "s",
            "conversion s.cv",
        ),
        (
            "CREATE SCHEMA s; CREATE TABLE t2 (a int, b int);
             CREATE STATISTICS s.st ON a, b FROM t2;",
            "s",
            "statistics object s.st",
        ),
        (
            "CREATE SCHEMA s; CREATE TABLE t2 (a int, b int);
             CREATE STATISTICS s.st ON a, b FROM t2; CREATE TABLE s.t3 (a int);",
            "s",
            "statistics object s.st",
        ),
        (
            "CREATE SCHEMA s; CREATE TABLE s.t3 (a int, b int);
             CREATE STATISTICS s.st3 ON a, b FROM s.t3;",
            "s",
            "table s.t3",
        ),
        (
            "CREATE SCHEMA \"M s\"; CREATE TABLE t2 (a int, b int);
             CREATE STATISTICS \"M s\".\"S t\" ON a, b FROM t2;",
            "\"M s\"",
            "statistics object \"M s\".\"S t\"",
        ),
        (
            "CREATE CONVERSION \"C v\" FOR 'UTF8' TO 'LATIN1' FROM utf8_to_iso8859_1;",
            "public",
            "conversion \"C v\"",
        ),
        (
            "CREATE SCHEMA s; CREATE EXTENSION citext SCHEMA s;",
            "s",
            "extension citext",
        ),
        (
            "CREATE SCHEMA s; CREATE EXTENSION hstore SCHEMA s;",
            "s",
            "extension hstore",
        ),
    ] {
        let shown = schema.trim_matches('"');
        assert_drop_fails(
            setup,
            &format!("DROP SCHEMA {schema};"),
            &format!(
                "cannot drop schema {shown} because other objects depend on it ({oldest} \
                 depends on schema {shown})"
            ),
        );
    }
    // CASCADE takes an extension in it along.
    build(&[
        (
            "0001.sql",
            "CREATE SCHEMA s; CREATE EXTENSION hstore SCHEMA s;",
        ),
        (
            "0002.sql",
            "DROP SCHEMA s CASCADE; CREATE EXTENSION hstore;",
        ),
    ]);
    assert_drop_fails(
        "CREATE SCHEMA s; CREATE EXTENSION citext SCHEMA s;",
        "DROP SCHEMA s CASCADE; DROP EXTENSION citext;",
        "extension \"citext\" does not exist",
    );
}

/// An extension's member that a cascade reaches takes the whole extension
/// along (findDependentObjects deletes its owning object instead), and
/// without CASCADE the extension is what is reported.
#[test]
fn a_cascade_reaching_an_extension_member_drops_the_extension() {
    let member = "CREATE EXTENSION citext; CREATE SCHEMA s;
                  CREATE TYPE ty AS (a int);
                  CREATE FUNCTION f(ty, int) RETURNS bool LANGUAGE sql AS 'select true';
                  CREATE OPERATOR s.=== (leftarg = ty, rightarg = int, function = f);
                  ALTER EXTENSION citext ADD OPERATOR s.===(ty, int);
                  CREATE TABLE tt (a int, c citext);";
    for drop in [
        "DROP SCHEMA s CASCADE;",
        "DROP FUNCTION f(ty, int) CASCADE;",
        "DROP TYPE ty CASCADE;",
    ] {
        assert_drop_fails(
            member,
            &format!("{drop} DROP EXTENSION citext;"),
            "extension \"citext\" does not exist",
        );
        // The extension's own dependents went too.
        build(&[
            ("0001.sql", member),
            (
                "0002.sql",
                &format!("{drop} CREATE EXTENSION citext; ALTER TABLE tt ADD COLUMN c int;"),
            ),
        ]);
    }
    for (drop, message) in [
        (
            "DROP SCHEMA s;",
            "cannot drop schema s because other objects depend on it (extension citext depends \
             on schema s)",
        ),
        (
            "DROP FUNCTION f(ty, int);",
            "cannot drop function f(ty,integer) because other objects depend on it (extension \
             citext depends on function f(ty,integer))",
        ),
        (
            "DROP TYPE ty;",
            "cannot drop type ty because other objects depend on it (function f(ty,integer) \
             depends on type ty)",
        ),
        (
            "DROP OPERATOR s.===(ty, int) CASCADE;",
            "cannot drop operator s.===(ty,integer) because extension citext requires it",
        ),
    ] {
        assert_drop_fails(member, drop, message);
    }
    // The oldest object in the schema comes first, the member included.
    assert_drop_fails(
        "CREATE EXTENSION citext; CREATE SCHEMA s; CREATE TABLE s.x (a int);
         CREATE FUNCTION f(int, int) RETURNS bool LANGUAGE sql AS 'select true';
         CREATE OPERATOR s.=== (leftarg = int, rightarg = int, function = f);
         ALTER EXTENSION citext ADD OPERATOR s.===(int, int);",
        "DROP SCHEMA s;",
        "cannot drop schema s because other objects depend on it (table s.x depends on schema s)",
    );
    // A plain operator goes with the function a type's CASCADE takes.
    build(&[
        (
            "0001.sql",
            "CREATE TYPE ty2 AS (a int);
             CREATE FUNCTION g(ty2, int) RETURNS bool LANGUAGE sql AS 'select true';
             CREATE OPERATOR ==== (leftarg = ty2, rightarg = int, function = g);",
        ),
        (
            "0002.sql",
            "DROP TYPE ty2 CASCADE;
             CREATE TYPE ty2 AS (a int);
             CREATE FUNCTION g(ty2, int) RETURNS bool LANGUAGE sql AS 'select true';
             CREATE OPERATOR ==== (leftarg = ty2, rightarg = int, function = g);",
        ),
    ]);
}

#[test]
fn drop_function_does_not_drop_an_aggregate() {
    let routines = "CREATE AGGREGATE ag(int) (sfunc = int4pl, stype = int);
                    CREATE PROCEDURE p(int) LANGUAGE sql AS 'select 1';
                    CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
                    CREATE SCHEMA s;
                    CREATE AGGREGATE s.ag2(int) (sfunc = int4pl, stype = int);";
    for (stmt, message) in [
        ("DROP FUNCTION ag(int);", "\"ag\" is an aggregate function"),
        (
            "DROP FUNCTION IF EXISTS ag(int);",
            "\"ag\" is an aggregate function",
        ),
        ("DROP FUNCTION ag;", "\"ag\" is an aggregate function"),
        (
            "DROP FUNCTION s.ag2(int);",
            "\"s.ag2\" is an aggregate function",
        ),
        (
            "DROP FUNCTION f(int), ag(int);",
            "\"ag\" is an aggregate function",
        ),
        ("DROP PROCEDURE ag(int);", "ag(integer) is not a procedure"),
        ("DROP PROCEDURE f(int);", "f(integer) is not a procedure"),
        ("DROP FUNCTION p(int);", "p(integer) is not a function"),
        (
            "DROP AGGREGATE f(int);",
            "function f(integer) is not an aggregate",
        ),
    ] {
        assert_drop_fails(routines, stmt, message);
    }
    build(&[
        ("0001.sql", routines),
        (
            "0002.sql",
            "DROP ROUTINE ag(int); DROP AGGREGATE s.ag2(int);",
        ),
    ]);
}
