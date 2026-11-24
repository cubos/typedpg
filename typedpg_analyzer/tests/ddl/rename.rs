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
        "cannot drop table t2 because other objects depend on it (view v depends on table t2)"
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

// ── RENAME COLUMN and the expressions reading the column ────────────────────

#[test]
fn a_renamed_column_is_read_by_its_constraints_under_its_new_name() {
    // PG keeps a CHECK by attnum: retyping the renamed column rebuilds the
    // CHECK over it (and fails where it no longer fits), whatever it was
    // called when the CHECK was written.
    let result = try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id int, x int, CHECK (x > 0), CHECK (t.x < 100));",
        ),
        ("0002.sql", "ALTER TABLE t RENAME COLUMN x TO y;"),
        ("0003.sql", "ALTER TABLE t ALTER COLUMN y TYPE text;"),
    ]);
    let err = result.expect_err("retyping the CHECKed column");
    assert!(
        err.to_string()
            .starts_with("operator does not exist: text > integer"),
        "{err}"
    );
    // So are an index's expressions and predicate.
    for (setup, msg) in [
        (
            "CREATE TABLE t (a int, b int); CREATE INDEX ON t ((a + 1));",
            "operator does not exist: text + integer",
        ),
        (
            "CREATE TABLE t (a int, b int); CREATE INDEX ON t (b) WHERE a > 0;",
            "operator does not exist: text > integer",
        ),
    ] {
        let err = try_apply(&[
            ("0001.sql", setup),
            ("0002.sql", "ALTER TABLE t RENAME COLUMN a TO x;"),
            ("0003.sql", "ALTER TABLE t ALTER COLUMN x TYPE text;"),
        ])
        .expect_err(setup);
        assert!(err.to_string().starts_with(msg), "{setup}: {err}");
    }
    // A column taking the old name is another column.
    build_db(&[
        (
            "0001.sql",
            "CREATE TABLE t (id int, x int, y text, CHECK (x > 0));",
        ),
        (
            "0002.sql",
            "ALTER TABLE t RENAME COLUMN x TO z;
             ALTER TABLE t RENAME COLUMN y TO x;
             ALTER TABLE t ALTER COLUMN x TYPE varchar(10);",
        ),
    ]);
}

#[test]
fn a_constraint_reads_its_table_however_it_was_qualified() {
    // PG stores a CHECK's columns, not the name it qualified them with:
    // retyping a column rebuilds `t.a > 0` over the renamed table, or over
    // a child the CHECK was copied to.
    build_db(&[
        (
            "0001.sql",
            "CREATE TABLE t (a int CHECK (t.a > 0), b int, c int GENERATED ALWAYS AS (t.b * 2) STORED);
             CREATE TABLE p (a int CHECK (p.a > 0));
             CREATE TABLE ch () INHERITS (p);
             CREATE TABLE pp (k int, a int CHECK (pp.a > 0)) PARTITION BY RANGE (k);
             CREATE TABLE w (a int, b int);
             CREATE INDEX ON w ((w.a + 1));
             CREATE TABLE pp1 PARTITION OF pp FOR VALUES FROM (0) TO (10);",
        ),
        (
            "0002.sql",
            "ALTER TABLE t RENAME TO u;
             ALTER TABLE u ALTER COLUMN a TYPE bigint;
             ALTER TABLE p ALTER COLUMN a TYPE bigint;
             ALTER TABLE pp ALTER COLUMN a TYPE bigint;
             ALTER TABLE w RENAME TO w2;
             ALTER TABLE w2 ALTER COLUMN a TYPE bigint;",
        ),
    ]);
}

#[test]
fn stored_expressions_call_renamed_and_moved_routines() {
    // PG keeps a CHECK's, an index's and a default's functions by OID:
    // ALTER COLUMN TYPE rebuilds them from pg_get_constraintdef /
    // pg_get_indexdef, which name the functions as they are now called
    // (schema-qualified when not visible) — renamed, moved, or in a
    // renamed schema.
    build_db(&[
        (
            "0001.sql",
            "CREATE SCHEMA s;
             CREATE FUNCTION pos(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
             CREATE FUNCTION inc(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
             CREATE FUNCTION s.spos(int) RETURNS bool LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             CREATE FUNCTION dflt() RETURNS int LANGUAGE sql AS 'SELECT 1';
             CREATE TABLE t (a int CHECK (pos(a)), b int, c int CHECK (s.spos(c)),
                 d int DEFAULT dflt());
             CREATE INDEX i1 ON t (inc(b)) WHERE pos(b);
             CREATE INDEX i2 ON t (c) WHERE s.spos(c);",
        ),
        (
            "0002.sql",
            "ALTER FUNCTION pos(int) RENAME TO pos2;
             ALTER FUNCTION inc(int) SET SCHEMA s;
             ALTER FUNCTION dflt() RENAME TO dflt2;
             ALTER SCHEMA s RENAME TO s2;
             ALTER TABLE t ALTER a TYPE smallint;
             ALTER TABLE t ALTER b TYPE smallint;
             ALTER TABLE t ALTER c TYPE smallint;
             ALTER TABLE t ALTER d TYPE bigint;
             CREATE TABLE c () INHERITS (t);
             CREATE TABLE l (LIKE t INCLUDING ALL);",
        ),
    ]);
}

#[test]
fn stored_expressions_name_renamed_and_moved_types_collations_and_operators() {
    // The same for the types a stored expression casts to, the collations
    // it names and the operators it calls.
    build_db(&[
        (
            "0001.sql",
            "CREATE SCHEMA s;
             CREATE TYPE e AS ENUM ('x', 'y');
             CREATE TYPE f AS ENUM ('x', 'y');
             CREATE COLLATION mycoll FROM \"C\";
             CREATE FUNCTION ieq(int, int) RETURNS bool LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 = $2';
             CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = ieq);
             CREATE TABLE t (a text CHECK (a::e IS NOT NULL), b text CHECK (b::f IS NOT NULL),
                 c text CHECK (c COLLATE mycoll > 'a'), x int, y int CHECK (x === y));
             CREATE INDEX ic ON t ((c COLLATE mycoll));
             CREATE INDEX io ON t (x) WHERE x === y;",
        ),
        (
            "0002.sql",
            "ALTER TYPE e RENAME TO e2;
             ALTER TYPE f SET SCHEMA s;
             ALTER COLLATION mycoll RENAME TO mycoll2;
             ALTER COLLATION mycoll2 SET SCHEMA s;
             ALTER OPERATOR === (int, int) SET SCHEMA s;
             ALTER TABLE t ALTER a TYPE varchar;
             ALTER TABLE t ALTER b TYPE varchar;
             ALTER TABLE t ALTER c TYPE varchar;
             ALTER TABLE t ALTER x TYPE int;
             ALTER TABLE t ALTER y TYPE int;
             CREATE TABLE c () INHERITS (t);",
        ),
    ]);
}

#[test]
fn an_expression_index_keeps_its_column_names_until_rebuilt() {
    // An index column over an expression is named when the index is
    // created (`inc` for `inc(a)`): renaming the function leaves the name,
    // and a rebuild by ALTER COLUMN TYPE names it anew from
    // pg_get_indexdef (`inc2`).
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE FUNCTION inc(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
         CREATE TABLE t (a int);
         CREATE UNIQUE INDEX i ON t (inc(a));
         ALTER FUNCTION inc(int) RENAME TO inc2;",
    )]);
    // ALTER TABLE ... RENAME COLUMN of the index finds the column by its
    // name.
    let missing = |db: &mut PgCatalog, name: &str| {
        let sql = format!("ALTER TABLE i RENAME COLUMN {name} TO k;");
        let err = db.apply_sql(&sql).expect_err(&sql);
        assert!(
            err.to_string()
                .starts_with(&format!("column \"{name}\" does not exist")),
            "got: {err}"
        );
    };
    missing(&mut db, "inc2");
    db.apply_sql("ALTER TABLE t ALTER a TYPE smallint;")
        .unwrap();
    missing(&mut db, "inc");
    db.apply_sql("ALTER TABLE i RENAME COLUMN inc2 TO k;")
        .unwrap();
}

#[test]
fn renaming_a_table_over_an_array_type_moves_the_array_aside() {
    // PG 19 RenameRelationInternal → RenameTypeInternal: the array type
    // `_a` (of table a) in the way moves to `__a`, then b's own array type
    // follows its new name: `__a` is taken, so `__a_1`.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE a (id int);
         CREATE TABLE b (c int);
         ALTER TABLE b RENAME TO _a;",
    )]);
    let a = db.resolve_type_by_name(Some("public"), "a").unwrap().oid;
    let b = db.resolve_type_by_name(Some("public"), "_a").unwrap().oid;
    assert_eq!(
        db.resolve_type_by_name(Some("public"), "__a")
            .and_then(|t| t.typelem),
        Some(a)
    );
    assert_eq!(
        db.resolve_type_by_name(Some("public"), "__a_1")
            .and_then(|t| t.typelem),
        Some(b)
    );
}

/// Objects in schema `s`, which the search path names only where a test
/// sets it.
const BOUND_SETUP: &str = "CREATE SCHEMA s;
     CREATE FUNCTION s.spos(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
     CREATE FUNCTION s.inc(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
     CREATE FUNCTION s.ieq(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 = $2';
     CREATE OPERATOR s.=== (LEFTARG = int, RIGHTARG = int, FUNCTION = s.ieq);
     CREATE TYPE s.e AS ENUM ('x', 'y');
     CREATE COLLATION s.co FROM \"C\";
     CREATE SEQUENCE s.sq;
     CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';";

/// Apply `sql`, which must fail with an error starting with `message`.
fn expect_failure(db: &mut PgCatalog, sql: &str, message: &str) {
    let err = db.apply_sql(sql).expect_err(sql);
    assert!(
        err.to_string().starts_with(message),
        "{sql}\n  expected: {message}\n       got: {err}"
    );
}

#[test]
fn stored_expressions_keep_the_objects_their_search_path_found() {
    // PG stores an expression's routines, operators, types, collations and
    // regclass constants as the OIDs the creation search path found: with
    // `s` off the path later, a CHECK, an index, a default, a generation
    // or partition key expression, a domain's CHECK and DEFAULT, a trigger
    // WHEN and a policy still run them, and ALTER COLUMN TYPE rebuilds
    // them (naming them `s.spos`, `OPERATOR(s.===)`, `s.e`, `s.co`).
    let mut db = build_db(&[
        ("0001.sql", BOUND_SETUP),
        (
            "0002.sql",
            "SET search_path = s, public;
             CREATE TABLE public.t (a int CHECK (spos(a)), b text CHECK (b::e IS NOT NULL),
                 c text CHECK (c COLLATE co > 'a'), x int, y int CHECK (x === y),
                 n int DEFAULT nextval('sq'), m int DEFAULT nextval('sq'::regclass),
                 w int, g bool GENERATED ALWAYS AS (spos(w)) STORED);
             CREATE INDEX ON public.t (inc(a)) WHERE spos(x);
             CREATE TABLE public.pk (a int) PARTITION BY RANGE (inc(a));
             CREATE DOMAIN public.d AS int CHECK (spos(VALUE)) DEFAULT inc(1);
             CREATE TABLE public.r (q int);
             CREATE TRIGGER tr BEFORE INSERT ON public.r FOR EACH ROW
                 WHEN (spos(new.q)) EXECUTE FUNCTION public.tf();
             CREATE POLICY p ON public.r USING (spos(q) AND q === q);",
        ),
    ]);
    db.apply_sql(
        "RESET search_path;
         ALTER TABLE t ALTER a TYPE smallint;
         ALTER TABLE t ALTER b TYPE varchar;
         ALTER TABLE t ALTER c TYPE varchar;
         ALTER TABLE t ALTER x TYPE int;
         ALTER TABLE t ALTER n TYPE bigint;
         ALTER TABLE t ALTER g TYPE text;
         CREATE TABLE c () INHERITS (t);
         CREATE TABLE l (LIKE t INCLUDING ALL);
         CREATE TABLE pk1 PARTITION OF pk FOR VALUES FROM (0) TO (10);
         CREATE TABLE td (v d);",
    )
    .unwrap();
    db.apply_sql(
        "INSERT INTO t (a, b, c, x, y) VALUES (1, 'x', 'b', 1, 1);
         INSERT INTO pk VALUES (1);
         INSERT INTO td DEFAULT VALUES;
         INSERT INTO r VALUES (1);",
    )
    .unwrap();
}

#[test]
fn an_object_created_earlier_on_the_search_path_does_not_capture_a_stored_name() {
    // A function or type created later in a schema that comes first on the
    // search path takes the name over for new statements, not for the
    // expressions bound to `public`'s: their rebuild names `public.f`, and
    // the error it can raise says so.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE FUNCTION f(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
         CREATE TYPE k AS ENUM ('x');
         CREATE TABLE t (a int CHECK (f(a)), b text CHECK (b::k IS NOT NULL));
         CREATE SCHEMA s2;
         CREATE FUNCTION s2.f(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT false';
         CREATE TYPE s2.k AS ENUM ('z');",
    )]);
    db.apply_sql(
        "SET search_path = s2, public;
         ALTER TABLE public.t ALTER a TYPE int;
         ALTER TABLE public.t ALTER b TYPE varchar;
         INSERT INTO public.t VALUES (1, 'x');",
    )
    .unwrap();
    expect_failure(
        &mut db,
        "ALTER TABLE public.t ALTER a TYPE text;",
        "function public.f(text) does not exist",
    );
    expect_failure(
        &mut db,
        "DROP FUNCTION public.f(int);",
        "cannot drop function public.f(integer) because other objects depend on it",
    );
    db.apply_sql("DROP FUNCTION s2.f(int); DROP TYPE s2.k;")
        .unwrap();
}

#[test]
fn a_rebuilt_expression_names_its_objects_as_the_search_path_shows_them() {
    // ruleutils qualifies a routine or an operator only when an unqualified
    // lookup under the current search path wouldn't find it: the error a
    // rebuild raises names it that way.
    let mut db = build_db(&[
        ("0001.sql", BOUND_SETUP),
        (
            "0002.sql",
            "SET search_path = s, public;
             CREATE TABLE public.t (a int CHECK (spos(a)), x int, y int CHECK (x === y));",
        ),
    ]);
    for (sql, message) in [
        (
            "ALTER TABLE public.t ALTER a TYPE text;",
            "function spos(text) does not exist",
        ),
        (
            "ALTER TABLE public.t ALTER x TYPE text;",
            "operator does not exist: text === integer",
        ),
    ] {
        expect_failure(&mut db, sql, message);
    }
    db.apply_sql("RESET search_path;").unwrap();
    for (sql, message) in [
        (
            "ALTER TABLE t ALTER a TYPE text;",
            "function s.spos(text) does not exist",
        ),
        (
            "ALTER TABLE t ALTER x TYPE text;",
            "operator does not exist: text s.=== integer",
        ),
    ] {
        expect_failure(&mut db, sql, message);
    }
}

#[test]
fn a_bound_expression_follows_its_objects_renamed_and_moved() {
    // The binding is to the object: renamed, moved to another schema or in
    // a renamed schema, it is still the one a rebuild names.
    let mut db = build_db(&[
        ("0001.sql", BOUND_SETUP),
        (
            "0002.sql",
            "SET search_path = s, public;
             CREATE TABLE public.t (a int CHECK (spos(a)), b int DEFAULT nextval('sq'));
             CREATE INDEX ON public.t (inc(a));",
        ),
    ]);
    db.apply_sql(
        "RESET search_path;
         ALTER FUNCTION s.spos(int) RENAME TO spos2;
         ALTER FUNCTION s.inc(int) SET SCHEMA public;
         ALTER SEQUENCE s.sq RENAME TO sq2;
         ALTER SCHEMA s RENAME TO s3;
         ALTER TABLE t ALTER a TYPE int;
         ALTER TABLE t ALTER b TYPE bigint;
         INSERT INTO t (a) VALUES (1);",
    )
    .unwrap();
    expect_failure(
        &mut db,
        "ALTER TABLE t ALTER a TYPE text;",
        "function inc(text) does not exist",
    );
}

#[test]
fn on_conflict_infers_an_index_bound_to_another_schema() {
    // ON CONFLICT compares the index's expressions as they print under the
    // current search path.
    let mut db = build_db(&[
        ("0001.sql", BOUND_SETUP),
        (
            "0002.sql",
            "SET search_path = s, public;
             CREATE TABLE public.u (a int);
             CREATE UNIQUE INDEX ON public.u (inc(a)) WHERE spos(a);",
        ),
    ]);
    db.apply_sql(
        "INSERT INTO public.u VALUES (1) ON CONFLICT (inc(a)) WHERE spos(a) DO NOTHING;
         RESET search_path;
         INSERT INTO u VALUES (1) ON CONFLICT (s.inc(a)) WHERE s.spos(a) DO NOTHING;",
    )
    .unwrap();
}

#[test]
fn a_rebuilt_expression_keeps_the_coercions_ruleutils_prints() {
    // ALTER COLUMN TYPE rebuilds a CHECK or an index from the text
    // pg_get_constraintdef / pg_get_indexdef print for it: a function's or
    // operator's argument shows its implicit coercion as a cast
    // (`pos((b)::integer)`, `length((c)::text)`, `((d)::text = 'x'::text)`),
    // which the new type then goes through explicitly — while a constant
    // is printed typed (`a > 5` for `a > '5'`).
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE FUNCTION pos(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
         CREATE FUNCTION inc(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
         CREATE TABLE t (b smallint CHECK (pos(b)), a int CHECK (pos(a)), x smallint);
         CREATE INDEX ON t (inc(x));
         CREATE TABLE v (c varchar(10) CHECK (length(c) > 0), d varchar CHECK (d = 'x'),
             e varchar);
         CREATE UNIQUE INDEX ON v (lower(e));
         CREATE TABLE k (a int CHECK (a > '5'));",
    )]);
    db.apply_sql(
        "ALTER TABLE t ALTER b TYPE text;
         ALTER TABLE t ALTER a TYPE smallint;
         ALTER TABLE t ALTER a TYPE text;
         ALTER TABLE t ALTER x TYPE text;
         ALTER TABLE v ALTER c TYPE int USING length(c);
         ALTER TABLE v ALTER d TYPE int USING length(d);
         ALTER TABLE v ALTER e TYPE int USING length(e);",
    )
    .unwrap();
    expect_failure(
        &mut db,
        "ALTER TABLE k ALTER a TYPE text;",
        "operator does not exist: text > integer",
    );
    db.apply_sql("ALTER TABLE k ALTER a TYPE bigint;").unwrap();
}

#[test]
fn inference_compares_an_index_rebuilt_with_coercions_as_transformed() {
    // ON CONFLICT's inference compares the transformed trees: a rebuilt
    // `lower((e)::text)` is the query's `lower(e)`, and a rebuilt
    // `((c)::text = 'x'::text)` predicate is implied by `c = 'x'`.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE u (a int, c varchar(10), e varchar);
         CREATE UNIQUE INDEX ON u (lower(e));
         CREATE UNIQUE INDEX ON u (a) WHERE c = 'x';",
    )]);
    db.apply_sql(
        "ALTER TABLE u ALTER e TYPE varchar(20);
         ALTER TABLE u ALTER c TYPE varchar(30);
         INSERT INTO u VALUES (1, 'x', 'e') ON CONFLICT (lower(e)) DO NOTHING;
         INSERT INTO u VALUES (1, 'x', 'e') ON CONFLICT (lower(e::text)) DO NOTHING;
         INSERT INTO u VALUES (1, 'x', 'e') ON CONFLICT (a) WHERE c = 'x' DO NOTHING;",
    )
    .unwrap();
}

#[test]
fn reg_constants_keep_the_objects_they_named() {
    // PG stores a reg* constant as an OID: another search path, a renamed
    // routine, type, text search configuration or schema leave it naming
    // the same object, and a rebuild prints it as its output function
    // does.
    let mut db = build_db(&[
        ("0001.sql", BOUND_SETUP),
        (
            "0002.sql",
            "CREATE TEXT SEARCH CONFIGURATION s.myconf (COPY = simple);
             SET search_path = s, public;
             CREATE TABLE public.r (p regproc DEFAULT 'spos',
                 q regprocedure DEFAULT 'spos(integer)',
                 o regoperator DEFAULT '===(integer,integer)', oo regoper DEFAULT '===',
                 y regtype DEFAULT 'e', co regcollation DEFAULT 'co',
                 n regnamespace DEFAULT 's', cf regconfig DEFAULT 'myconf',
                 y2 regtype CHECK (y2 <> 'e'::regtype),
                 q2 regprocedure CHECK (q2 <> 'spos(integer)'::regprocedure));",
        ),
    ]);
    db.apply_sql(
        "RESET search_path;
         INSERT INTO r (y2, q2) VALUES ('int4', 'tf()');
         ALTER TABLE r ALTER y2 TYPE regtype;
         ALTER TABLE r ALTER q2 TYPE regprocedure;
         ALTER FUNCTION s.spos(int) RENAME TO spos2;
         ALTER TYPE s.e RENAME TO e2;
         ALTER TEXT SEARCH CONFIGURATION s.myconf RENAME TO myconf2;
         ALTER SCHEMA s RENAME TO s3;
         INSERT INTO r (y2, q2) VALUES ('int4', 'tf()');
         ALTER TABLE r ALTER y2 TYPE regtype;
         ALTER TABLE r ALTER q2 TYPE regprocedure;",
    )
    .unwrap();
}

#[test]
fn role_and_database_constants_cannot_be_stored() {
    // find_expr_references_walker records no dependency on a role or a
    // database, so a stored expression may not hold a `regrole` or
    // `regdatabase` constant.
    let mut db = build_db(&[("0001.sql", "CREATE ROLE stored_regrole_owner;")]);
    for sql in [
        "CREATE TABLE r (g regrole DEFAULT 'stored_regrole_owner');",
        "CREATE TABLE r (g regrole CHECK (g <> 'stored_regrole_owner'::regrole));",
        "CREATE TABLE r (g oid CHECK (g <> 'stored_regrole_owner'::regrole::oid));",
    ] {
        expect_failure(
            &mut db,
            sql,
            "constant of the type regrole cannot be used here",
        );
    }
    db.apply_sql("CREATE TABLE r (g regrole, n int CHECK (n > 0));")
        .unwrap();
}

/// A table and the objects the expression tests below read.
const EXPR_SETUP: &str =
    "CREATE FUNCTION pos(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
     CREATE FUNCTION inc(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
     CREATE FUNCTION vsum(VARIADIC int[]) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 1';
     CREATE FUNCTION named(x int, y int DEFAULT 0) RETURNS int LANGUAGE sql IMMUTABLE
         AS 'SELECT 1';
     CREATE TYPE ct AS (x int, y text);
     CREATE DOMAIN dint AS int;
     CREATE DOMAIN dbool AS bool;
     CREATE SCHEMA s;
     CREATE COLLATION s.co FROM \"C\";
     CREATE FUNCTION s.ieq(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 = $2';
     CREATE OPERATOR s.=== (LEFTARG = int, RIGHTARG = int, FUNCTION = s.ieq);
     CREATE TABLE t (a int, b smallint, c varchar(10), d text, e numeric, f bool, g int[],
         h date, k ct, dm dint, bd dbool);";

#[test]
fn stored_expressions_print_as_pg_get_expr_does() {
    // The pg_sanity mirror compares the text the analyzer would rebuild
    // each CHECK, index expression and predicate from with what
    // pg_get_expr / pg_get_indexdef print: implicit coercions shown where
    // ruleutils shows them and hidden where it hides them, constants
    // typed, CASE laid out on lines, SQL syntax as SQL syntax.
    let checks = [
        "a > '5'",
        "pos(b)",
        "length(c) > 0",
        "c = 'x'",
        "NOT f",
        "bd",
        "bd AND f",
        "f IS NOT UNKNOWN",
        "a IS DISTINCT FROM b",
        "a IS NOT DISTINCT FROM b",
        "COALESCE(a, b, 0) > 0",
        "GREATEST(a, b) > LEAST(a, 1)",
        "NULLIF(c, 'x') IS NULL",
        "CASE WHEN a > 0 THEN b END > 0",
        "CASE a WHEN 1 THEN 'x' WHEN 2 THEN 'y' END = 'x'",
        "CASE WHEN a > 0 THEN CASE WHEN b > 0 THEN 1 ELSE 2 END ELSE 3 END > 0",
        "c IN ('x', 'y')",
        "a <> ALL (ARRAY[1, 2])",
        "g[1:2] IS NOT NULL",
        "(ARRAY[a, b])[1] > 0",
        "(k).y IS NOT NULL",
        "ROW(a, d)::ct IS NOT NULL",
        "d COLLATE s.co > 'a'",
        "a OPERATOR(s.===) a",
        "vsum(a, b, 3) > 0",
        "vsum(VARIADIC ARRAY[a, b]) > 0",
        "named(y => b, x => a) > 0",
        "d ILIKE 'x%'",
        "d NOT SIMILAR TO 'x%'",
        "h > '2020-01-01'",
        "EXTRACT(year FROM h) > 2000",
        "substring(d FROM 1 FOR 2) <> ''",
        "trim(LEADING 'x' FROM d) <> ''",
        "position('x' IN d) > 0",
        "(h AT TIME ZONE 'UTC') IS NOT NULL",
        "e::numeric(10,2) > 0",
        "d::char(3) <> ''",
        "dm > 0",
    ];
    let mut sql = String::new();
    for (i, check) in checks.iter().enumerate() {
        sql.push_str(&format!(
            "ALTER TABLE t ADD CONSTRAINT k{i} CHECK ({check});\n"
        ));
    }
    sql.push_str(
        "CREATE INDEX i1 ON t ((a::bigint), NULLIF(a, 0), lower(c), (CASE WHEN f THEN a ELSE b END));
         CREATE INDEX i2 ON t (a) WHERE c = 'x' AND bd;",
    );
    build_db(&[("0001.sql", EXPR_SETUP), ("0002.sql", &sql)]);
}

#[test]
fn a_rebuild_keeps_the_coercions_inside_case_coalesce_and_calls() {
    // ruleutils shows a CASE result's, a COALESCE argument's, a variadic
    // element's and a named argument's coercion as a cast: the rebuild
    // over the new type goes through it explicitly.
    let mut db = build_db(&[
        ("0001.sql", EXPR_SETUP),
        (
            "0002.sql",
            "CREATE TABLE r (b smallint, x smallint, y smallint, z smallint, f bool,
                 CHECK (COALESCE(b, 0) > 0), CHECK (CASE WHEN f THEN x ELSE 0 END > 0),
                 CHECK (vsum(y, 1) > 0), CHECK (named(y => 1, x => z) > 0));",
        ),
    ]);
    db.apply_sql(
        "ALTER TABLE r ALTER b TYPE text;
         ALTER TABLE r ALTER x TYPE text;
         ALTER TABLE r ALTER y TYPE text;
         ALTER TABLE r ALTER z TYPE text;",
    )
    .unwrap();
}

#[test]
fn alter_type_subcommands_are_rebuilt_together() {
    // ATPostAlterTypeCleanup rebuilds what the statement's ALTER TYPE
    // subcommands changed after the last of them, each dependent's text
    // remembered before any column changed: `a = b` over two columns made
    // text together holds.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int, CHECK (a = b));
         CREATE INDEX ON t ((a + b));",
    )]);
    db.apply_sql("ALTER TABLE t ALTER a TYPE bigint, ALTER b TYPE bigint;")
        .unwrap();
    expect_failure(
        &mut db,
        "ALTER TABLE t ALTER a TYPE text, ALTER b TYPE text;",
        "operator does not exist: text + text",
    );
    db.apply_sql("DROP INDEX t_expr_idx; ALTER TABLE t ALTER a TYPE text, ALTER b TYPE text;")
        .unwrap();
}

#[test]
fn a_later_search_path_does_not_rebind_a_stored_expression() {
    // `a = 1` was bound to `pg_catalog.=` when written: a statement run
    // later with a schema holding another `=` first on the path leaves it
    // so, and its rebuild names `OPERATOR(pg_catalog.=)` under that path.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int CHECK (a = 1));
         CREATE SCHEMA loose;
         CREATE FUNCTION loose.eq(int, int) RETURNS boolean LANGUAGE sql IMMUTABLE
             AS 'SELECT true';
         CREATE OPERATOR loose.= (LEFTARG = int, RIGHTARG = int, FUNCTION = loose.eq);",
    )]);
    db.apply_sql(
        "SET search_path = loose, pg_catalog, public;
         CREATE TABLE public.u (b int CHECK (b = 1));
         ALTER TABLE public.t ALTER a TYPE bigint;",
    )
    .unwrap();
}

#[test]
fn a_retyped_column_rebuilds_the_indexes_selecting_it_as_a_field() {
    // `(atconst()).a` records a dependency on attbl2.a: ALTER COLUMN TYPE
    // rebuilds atref's index and atchk's CHECK from their text, the field
    // now bigint (PG prints `((atconst()).a)::numeric` after it).
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE attbl2 (a numeric, b text);
         CREATE FUNCTION atconst() RETURNS attbl2 LANGUAGE sql IMMUTABLE
             AS $$ SELECT 1::numeric, 'x'::text $$;
         CREATE TABLE atref (data text);
         CREATE INDEX ON atref (((atconst()).a + length(data)));
         CREATE TABLE atchk (n numeric, CHECK ((atconst()).a < n));",
    )]);
    db.apply_sql("ALTER TABLE attbl2 ALTER a TYPE bigint;")
        .unwrap();
    // The rebuilt texts cast the field to numeric now.
    expect_failure(
        &mut db,
        "ALTER TABLE attbl2 ALTER a TYPE point USING point(a, a);",
        "cannot cast type point to numeric",
    );
    db.apply_sql("DROP INDEX atref_expr_idx; ALTER TABLE attbl2 ALTER a TYPE text;")
        .unwrap();
}

#[test]
fn a_rename_under_a_shadowing_path_rebinds_nothing() {
    // The renames run with another schema's `g` and `=` first on the path:
    // the stored expressions still call `public.g` and `pg_catalog.=`.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1';
         CREATE TABLE t (a int CHECK (f(a) = 1));
         CREATE INDEX ON t (f(a)) WHERE f(a) = 1;
         CREATE POLICY p ON t USING (f(a) = 1);
         CREATE SCHEMA loose;
         CREATE FUNCTION loose.g(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 2';
         CREATE FUNCTION loose.eq(int, int) RETURNS boolean LANGUAGE sql IMMUTABLE
             AS 'SELECT true';
         CREATE OPERATOR loose.= (LEFTARG = int, RIGHTARG = int, FUNCTION = loose.eq);",
    )]);
    db.apply_sql(
        "SET search_path = loose, pg_catalog, public;
         ALTER FUNCTION public.f(int) RENAME TO g;
         ALTER TABLE public.t RENAME TO u;
         ALTER SCHEMA public RENAME TO pub;
         ALTER SCHEMA pub RENAME TO public;
         DROP POLICY p ON public.u;",
    )
    .unwrap();
    // The CHECK's rebuild calls `public.g(bigint)`, not `loose.g`.
    expect_failure(
        &mut db,
        "ALTER TABLE u ALTER a TYPE bigint;",
        "function public.g(bigint) does not exist",
    );
}

#[test]
fn stored_constants_print_through_their_output_functions() {
    // get_const_expr prints a constant through its type's output function:
    // the pg_sanity mirror compares every one of these with pg_get_expr
    // (PG 19 prints `'1.5'::double precision`, `'1 day 02:00:00'::interval`,
    // `'[2,6)'::int4range`, `'{"a": [1.50, 200, 0], …}'::jsonb`…).
    let mut db = build_db(&[(
        "0001.sql",
        r#"CREATE TABLE k (
  f8 float8 CHECK (f8 <> '1.50' AND f8 <> '1e20' AND f8 <> '-0.00001' AND f8 <> 'NaN'
      AND f8 <> '-inf' AND f8 <> '123456789012345678' AND f8 <> '0.1'),
  f4 float4 CHECK (f4 <> '3.14159265358979' AND f4 <> '1000000' AND f4 <> '100000'),
  n numeric CHECK (n <> '1.50' AND n <> '1e3' AND n <> '-0.0' AND n <> 'NaN' AND n <> 1.5e-3
      AND n <> '-12.50' AND n <> '-inf'),
  d date CHECK (d <> '2024-01-05' AND d <> '0044-03-15 BC' AND d <> 'infinity' AND d > 'epoch'),
  ts timestamp CHECK (ts <> '2024-01-05 10:30' AND ts <> '2024-01-05 10:30:00.250'
      AND ts <> '-infinity'),
  tz timestamptz CHECK (tz <> '2024-01-05 10:30+02' AND tz <> '2024-01-05 10:30:00.5-03:30'),
  tm time CHECK (tm <> '10:30' AND tm <> '23:59:59.5'),
  iv interval CHECK (iv <> '1 day 2 hours' AND iv <> '-1 day +2 hours' AND iv <> '14 months'
      AND iv <> '1.5 seconds' AND iv <> '0' AND iv <> '-1 mons -1.5 seconds'),
  byt bytea CHECK (byt <> '\xDEADbeef' AND byt <> 'abc\\d\001'),
  u uuid CHECK (u <> '{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}'
      AND u <> 'a0eebc999c0b4ef8bb6d6bb9bd380a11'),
  ip inet CHECK (ip <> '192.168.1.5/24' AND ip <> '10.0.0.1/32' AND ip <> '2001:DB8::1'
      AND ip <> '::ffff:1.2.3.4' AND ip <> '2001:db8:0:0:1:0:0:1' AND ip <> '::1'),
  cn cidr CHECK (cn <> '192.168.0.0/16' AND cn <> '2001:db8::/32' AND cn <> '10.1.2.3/32'),
  ma macaddr CHECK (ma <> '08-00-2B-01-02-03' AND ma <> '0800.2b01.0203'),
  m8 macaddr8 CHECK (m8 <> '08:00:2b:01:02:03' AND m8 <> '08002b0102030405'),
  bt bit(3) CHECK (bt <> B'101' AND bt <> '110'),
  vb varbit CHECK (vb <> X'1F' AND vb <> '1011'),
  mo money CHECK (mo <> '12.345' AND mo <> '-1234.5' AND mo <> '$1,000,000.00'),
  js jsonb CHECK (js <> '{"b": 1, "a": [1.50, 2e2, -0], "b": "xé\n", "aa": null, "c": {}}'
      AND js <> '1E2' AND js <> '"s"' AND js <> '[]'),
  pt point CHECK (pt ~= '(1.50, -2)'),
  bx box CHECK (bx = '(0,0),(2,3)' AND bx = '(5,1),(2,3)'),
  ls lseg CHECK (ls = '[(0,0),(1.5,2)]'),
  ci circle CHECK (ci = '<(1,2),3.5>' AND ci = '((1,2),3.5)'),
  ln line CHECK (ln = '{1,-1,0}'),
  pa path CHECK (pa = '[(0,0),(1,1)]' AND pa = '((0,0),(1,1),(2,0))'),
  pg polygon CHECK (pg ~= '((0,0),(1,0),(1,1))'),
  ar int[] CHECK (ar <> '{1,2,NULL}' AND ar <> '[0:1]={3,4}' AND ar <> '{{1,2},{3,4}}'
      AND ar <> '{}'),
  ta text[] CHECK (ta <> '{"a b", "", "NULL", "q\"x", plain}'),
  fa float8[] CHECK (fa <> '{1.50,1e20}'),
  r4 int4range CHECK (r4 <> '(1,5]' AND r4 <> '[3,3)' AND r4 <> '(,10)' AND r4 <> 'empty'),
  nr numrange CHECK (nr <> '[1.50,2.0)' AND nr <> '(1,1]'),
  dr daterange CHECK (dr <> '[2024-01-01,2024-01-31]'),
  tr tsrange CHECK (tr <> '[2024-01-01 10:00,2024-01-02)'),
  lsn pg_lsn CHECK (lsn <> '16/B374D848' AND lsn <> '0/1'),
  nm name CHECK (nm <> 'abc'),
  ch "char" CHECK (ch <> 'x'),
  o oid CHECK (o <> '42'),
  bo bool CHECK (bo <> 'yes'),
  i8 int8 CHECK (i8 <> ' 007')
);
CREATE INDEX ON k (f8) WHERE f8 > '2.50' AND iv < '1 day';"#,
    )]);
    // Dates and intervals print in the session's styles.
    db.apply_sql(
        "SET DateStyle = 'German';
         SET IntervalStyle = 'sql_standard';
         ALTER TABLE k ADD CHECK (d <> '2024-02-03' AND iv <> '2 hours');
         SET DateStyle = 'ISO';
         ALTER TABLE k ADD CHECK (d <> '2024-02-04');
         RESET IntervalStyle;
         ALTER TABLE k ADD CHECK (iv <> '3 hours');",
    )
    .unwrap();
}
