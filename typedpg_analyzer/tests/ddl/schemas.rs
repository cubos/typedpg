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

// ── Session identity: SET ROLE / SET SESSION AUTHORIZATION ─────────────────

#[test]
fn set_role_and_session_authorization_are_accepted() {
    // PG 18 accepts each of these for a role that exists (`postgres`, the
    // predefined roles); `NONE` / DEFAULT / RESET go back to no role.
    build_db(&[(
        "0001.sql",
        "SET ROLE postgres;
         SET ROLE NONE;
         SET ROLE 'none';
         RESET ROLE;
         SET ROLE TO DEFAULT;
         SET role = 'postgres';
         BEGIN;
         SET LOCAL ROLE postgres;
         COMMIT;
         SET ROLE pg_read_all_data;
         RESET ROLE;
         SET SESSION AUTHORIZATION postgres;
         SET SESSION AUTHORIZATION DEFAULT;
         SET session_authorization = postgres;
         RESET SESSION AUTHORIZATION;
         SET session_authorization TO DEFAULT;
         SELECT set_config('role', 'postgres', false);
         SELECT set_config('role', 'none', false);
         RESET ALL;",
    )]);
    // Roles that can't exist: `public` and `none` are reserved role names,
    // and so is every `pg_` name but the predefined roles.
    for stmt in [
        "SET ROLE public;",
        "SET ROLE \"public\";",
        "SET ROLE pg_foo;",
        "SET SESSION AUTHORIZATION none;",
        "SET SESSION AUTHORIZATION public;",
        "SET SESSION AUTHORIZATION pg_foo;",
        "SELECT set_config('role', 'pg_foo', false);",
        "CREATE SCHEMA AUTHORIZATION public;",
        "CREATE SCHEMA s AUTHORIZATION pg_foo;",
    ] {
        let err = try_apply(&[("0001.sql", stmt)]).expect_err(stmt);
        let role = if stmt.contains("none") {
            "none"
        } else if stmt.contains("public") {
            "public"
        } else {
            "pg_foo"
        };
        assert!(
            err.to_string()
                .starts_with(&format!("role \"{role}\" does not exist")),
            "{stmt}\n  got: {err}"
        );
    }
}

#[test]
fn user_in_search_path_follows_the_migration_role() {
    // PG 18: with the default search_path ("$user", public), a table
    // created under SET ROLE / SET SESSION AUTHORIZATION goes to the schema
    // named after the current user, when there is one.
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA postgres;
         SET ROLE postgres;
         CREATE TABLE ut (a int);
         RESET ROLE;
         SET SESSION AUTHORIZATION postgres;
         CREATE TABLE ut2 (a int);
         RESET SESSION AUTHORIZATION;",
    )]);
    for table in ["postgres.ut", "postgres.ut2"] {
        let info = db.analyze(&format!("SELECT * FROM {table}")).unwrap();
        assert_cols(&info, vec![cn("a", int4())]);
    }
    let err = db.analyze("SELECT * FROM public.ut").unwrap_err();
    assert!(matches!(err, AnalyzeError::UndefinedTable(_)), "{err:?}");
}

#[test]
fn unverifiable_role_names_are_assumed_to_exist() {
    // Roles live in the cluster, not in the catalog the migrations build:
    // a name that could be a role is taken as one (PG would reject it only
    // if the cluster has no such role, or the session may not take it).
    let mut db = PgCatalog::new().unwrap();
    db.skip_pg_sanity();
    db.apply_sql(
        "CREATE SCHEMA app_owner;
         SET ROLE app_owner;
         CREATE TABLE t (a int);",
    )
    .unwrap();
    // CURRENT_ROLE is app_owner, whose schema exists.
    let err = db
        .apply_sql("CREATE SCHEMA AUTHORIZATION CURRENT_ROLE;")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("schema \"app_owner\" already exists"),
        "{err}"
    );
    db.apply_sql(
        "SET SESSION AUTHORIZATION app_admin;
         CREATE SCHEMA AUTHORIZATION SESSION_USER;
         SET ROLE app_owner;
         CREATE TABLE t2 (a int);",
    )
    .unwrap();
    for table in ["app_owner.t", "app_owner.t2"] {
        let info = db.analyze(&format!("SELECT * FROM {table}")).unwrap();
        assert_cols(&info, vec![cn("a", int4())]);
    }
    assert!(db.resolve_table(Some("app_admin"), "t2").is_none());
    assert!(db.namespace_oid("app_admin").is_some());
    // The application's session is not the migrations': its `$user` is
    // unknown.
    assert!(db.resolve_table(None, "t2").is_none());
}
