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
