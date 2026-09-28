//! CREATE / DROP SCHEMA, RENAME SCHEMA, schema-qualified object handling,
//! CASCADE drops that remove all objects in a schema.

use crate::common::*;

// ── DROP SCHEMA ─────────────────────────────────────────────────────────────

#[test]
fn drop_schema_empty_succeeds() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA temp_stuff;
         DROP SCHEMA temp_stuff;",
    )]);
    assert!(snap.namespace_oid("temp_stuff").is_none());
}

#[test]
fn drop_schema_with_objects_fails_without_cascade() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE SCHEMA foo;
         CREATE TABLE foo.bar (id INT PRIMARY KEY);
         DROP SCHEMA foo;",
    )]);
    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cannot drop schema foo because other objects depend on it"
    );
}

#[test]
fn drop_schema_cascade_removes_all_contents() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA foo;
         CREATE TABLE foo.bar (id INT PRIMARY KEY, name TEXT NOT NULL);
         CREATE TYPE foo.my_enum AS ENUM ('a', 'b');
         CREATE FUNCTION foo.do_it(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         DROP SCHEMA foo CASCADE;",
    )]);

    assert!(snap.resolve_table(Some("foo"), "bar").is_none());
    assert!(snap.resolve_type_by_name(Some("foo"), "my_enum").is_none());
    assert!(snap.find_functions(Some("foo"), "do_it").is_empty());
}

#[test]
fn drop_schema_cascade_transitively_drops_views_in_other_schemas() {
    // A view in `public` depends on a table in `foo`. DROP SCHEMA foo
    // CASCADE must take the view down too.
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA foo;
         CREATE TABLE foo.items (id INT PRIMARY KEY, name TEXT NOT NULL);
         CREATE VIEW public.item_names AS SELECT id, name FROM foo.items;
         DROP SCHEMA foo CASCADE;",
    )]);

    assert!(snap.resolve_table(Some("public"), "item_names").is_none());
}

#[test]
fn drop_schema_if_exists_no_error() {
    let _snap = build(&[("0001.sql", "DROP SCHEMA IF EXISTS nonexistent;")]);
}

#[test]
fn drop_schema_missing_errors_without_if_exists() {
    let result = try_apply(&[("0001.sql", "DROP SCHEMA nonexistent;")]);
    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "schema \"nonexistent\" does not exist"
    );
}

// ── search_path set by migrations ───────────────────────────────────────────

#[test]
fn set_search_path_directs_where_objects_are_created() {
    // PG 18: app.t exists with column a integer; public.t does not (42P01).
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA app;
         SET search_path = app, public;
         CREATE TABLE t (a int);
         RESET search_path;",
    )]);
    let info = db.analyze("SELECT * FROM app.t").unwrap();
    assert_cols(&info, vec![cn("a", int4())]);
    let err = db.analyze("SELECT * FROM public.t").unwrap_err();
    assert!(matches!(err, AnalyzeError::UndefinedTable(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("relation \"public.t\" does not exist"),
        "{err}"
    );
}

#[test]
fn set_search_path_resolves_unqualified_types() {
    // PG 18: app.t2.m is app.mood.
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA app;
         CREATE TYPE app.mood AS ENUM ('x');
         SET search_path = app;
         CREATE TABLE t2 (m mood);
         RESET search_path;",
    )]);
    let info = db.analyze("SELECT m FROM app.t2").unwrap();
    assert_cols(&info, vec![cn("m", enum_ty("app", "mood", &["x"]))]);
}

#[test]
fn search_path_may_name_a_schema_created_later() {
    let db = build_db(&[(
        "0001.sql",
        "SET search_path = later, public;
         CREATE SCHEMA later;
         CREATE TABLE t3 (a int);
         RESET search_path;",
    )]);
    let info = db.analyze("SELECT * FROM later.t3").unwrap();
    assert_cols(&info, vec![cn("a", int4())]);
}

#[test]
fn set_config_search_path_in_select_is_applied() {
    // The form pg_dump emits.
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA app;
         SELECT pg_catalog.set_config('search_path', 'app, public', false);
         CREATE TABLE t4 (b text);
         RESET search_path;",
    )]);
    let info = db.analyze("SELECT * FROM app.t4").unwrap();
    assert_cols(&info, vec![cn("b", text())]);
}

#[test]
fn empty_search_path_has_no_creation_schema() {
    // PG 18: ERROR 3F000 no schema has been selected to create in.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "SET search_path = ''; CREATE TABLE t5 (a int);",)]),
        DdlError::Parse(_),
        "no schema has been selected to create in",
    );
}

#[test]
fn set_local_search_path_ends_with_the_transaction() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA app;
         BEGIN; SET LOCAL search_path = app; CREATE TABLE t6 (a int); COMMIT;
         CREATE TABLE t7 (a int);",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM app.t6").unwrap(),
        vec![cn("a", int4())],
    );
    assert_cols(
        &db.analyze("SELECT * FROM public.t7").unwrap(),
        vec![cn("a", int4())],
    );
}

#[test]
fn alter_table_finds_relations_along_the_search_path() {
    // PG 18: both ALTERs succeed — `t` resolves to app.t, `t4` to public.t4.
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA app;
         CREATE TABLE app.t (a int);
         CREATE TABLE public.t4 (b text);
         SET search_path = app, public;
         ALTER TABLE t ADD COLUMN z int;
         ALTER TABLE t4 ADD COLUMN y int;
         RESET search_path;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM app.t").unwrap(),
        vec![cn("a", int4()), cn("z", int4())],
    );
    assert_cols(
        &db.analyze("SELECT * FROM public.t4").unwrap(),
        vec![cn("b", text()), cn("y", int4())],
    );
}

// ── CREATE SCHEMA elements / duplicates ─────────────────────────────────────

#[test]
fn create_schema_elements_are_created_in_the_new_schema() {
    // PG 18: s.t (a integer), s.v (a integer); s4.v reads public.t.
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA s CREATE TABLE t (a int) CREATE VIEW v AS SELECT a FROM t;
         CREATE TABLE public.pt (b text);
         CREATE SCHEMA s4 CREATE VIEW v AS SELECT * FROM pt;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM s.t").unwrap(),
        vec![cn("a", int4())],
    );
    assert_cols(
        &db.analyze("SELECT * FROM s.v").unwrap(),
        vec![cn("a", int4())],
    );
    assert_cols(
        &db.analyze("SELECT * FROM s4.v").unwrap(),
        vec![cn("b", text())],
    );
    assert!(db.analyze("SELECT * FROM public.t").is_err());
}

#[test]
fn create_schema_errors() {
    // PG 18: 42P06 schema "s" already exists; 42P15 for a mismatched element.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE SCHEMA s; CREATE SCHEMA s;")]),
        DdlError::DuplicateObject(_),
        "schema \"s\" already exists",
    );
    build_db(&[(
        "0001.sql",
        "CREATE SCHEMA s; CREATE SCHEMA IF NOT EXISTS s;",
    )]);
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE SCHEMA s3 CREATE TABLE public.y (a int);"
        )]),
        DdlError::Parse(_),
        "CREATE specifies a schema (public) different from the one being created (s3)",
    );
}
