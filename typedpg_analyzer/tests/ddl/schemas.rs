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
fn create_schema_elements_run_grouped_by_kind() {
    // PG 18 transformCreateSchemaStmtElements: sequences, tables, views,
    // indexes, triggers, then grants, whatever order they are written in.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';",
        ),
        (
            "0002.sql",
            "CREATE SCHEMA s
                 GRANT SELECT ON v TO PUBLIC
                 CREATE TRIGGER tr AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION public.tf()
                 CREATE INDEX i ON t (a)
                 CREATE VIEW v AS SELECT a, b FROM t
                 CREATE TABLE t (a int, b bigint DEFAULT nextval('sq'))
                 CREATE SEQUENCE sq;",
        ),
    ]);
    assert_cols(
        &db.analyze("SELECT * FROM s.v").unwrap(),
        vec![cn("a", int4()), cn("b", int8())],
    );
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

#[test]
fn the_seed_keeps_the_server_search_path_setting() {
    // The seed records the server's setting as text, `$user` included; a
    // migration's SET lasts only for its session.
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA app; SET search_path = app; CREATE TABLE t (a int);",
    )]);
    let mut seed = db.to_seed();
    assert_eq!(seed.search_path, "\"$user\", public");

    // Another server setting is honored, in order.
    seed.search_path = "\"$user\", app, public".into();
    let restored = PgCatalog::from_seed(seed);
    let info = restored.analyze("SELECT a FROM t").unwrap();
    assert_eq!(info.columns.len(), 1);
}

#[test]
fn a_migration_session_search_path_does_not_reach_the_queries() {
    // SET search_path lasts for the migration runner's session — the
    // following migrations too — but the application's queries run in
    // sessions of their own, with the server's setting.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE SCHEMA app; SET search_path = app; CREATE TABLE t (a int);",
        ),
        ("0002.sql", "CREATE TABLE t2 (b int);"),
    ]);
    assert!(db.analyze("SELECT a FROM app.t").is_ok());
    assert!(db.analyze("SELECT b FROM app.t2").is_ok());
    let err = db.analyze("SELECT a FROM t").unwrap_err();
    assert!(
        err.to_string().starts_with("relation \"t\" does not exist"),
        "{err}"
    );
}

#[test]
fn alter_database_set_search_path_reaches_the_queries() {
    // ALTER DATABASE ... SET is the setting of the database's new sessions:
    // the application's, not the migrations' session running it. The
    // database name can't be checked (nor matched against the oracle's
    // scratch database), so any name is taken as the application's.
    let apply = |sql: &str| {
        let mut db = PgCatalog::new().unwrap();
        db.skip_pg_sanity();
        db.apply_sql(sql).map(|()| db)
    };
    let db = apply(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (a int);
         ALTER DATABASE appdb SET search_path = app, public;
         CREATE TABLE u (x int);",
    )
    .unwrap();
    assert!(db.analyze("SELECT a FROM t").is_ok());
    // The migrations' session kept its own setting: u went to public.
    assert!(db.analyze("SELECT x FROM public.u").is_ok());
    assert!(db.analyze("SELECT x FROM app.u").is_err());

    // It survives a seed round trip.
    let seed = db.to_seed();
    assert_eq!(seed.search_path, "\"app\", \"public\"");
    assert!(
        PgCatalog::from_seed(seed)
            .analyze("SELECT a FROM t")
            .is_ok()
    );

    // FROM CURRENT takes the session's value; RESET drops the setting.
    let db = apply(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (a int);
         SET search_path = app;
         ALTER DATABASE appdb SET search_path FROM CURRENT;
         RESET search_path;",
    )
    .unwrap();
    assert!(db.analyze("SELECT a FROM t").is_ok());
    for reset in [
        "ALTER DATABASE appdb RESET search_path;",
        "ALTER DATABASE appdb RESET ALL;",
        "ALTER DATABASE appdb SET search_path TO DEFAULT;",
    ] {
        let db = apply(&format!(
            "CREATE SCHEMA app;
             CREATE TABLE app.t (a int);
             ALTER DATABASE appdb SET search_path = app;
             {reset}"
        ))
        .unwrap();
        let err = db.analyze("SELECT a FROM t").unwrap_err();
        assert!(
            err.to_string().starts_with("relation \"t\" does not exist"),
            "{reset}: {err}"
        );
    }

    // Parameters are checked as for SET.
    for (sql, message) in [
        (
            "ALTER DATABASE appdb SET nosuch = 1;",
            "unrecognized configuration parameter \"nosuch\"",
        ),
        (
            "ALTER DATABASE appdb SET work_mem = 'lots';",
            "invalid value for parameter \"work_mem\": \"lots\"",
        ),
    ] {
        let err = apply(sql).err().expect(sql);
        assert!(err.to_string().starts_with(message), "{sql}\n  got: {err}");
    }
}

#[test]
fn set_schema_moves_a_relation_with_its_indexes_and_sequences() {
    // AlterTableNamespace: the names must be free in the new schema — the
    // relation's, its row type's, its indexes' and owned sequences'; an
    // owned sequence moves only with its table; nothing moves in to or out
    // of the temporary or TOAST schema.
    let setup = "CREATE SCHEMA s;
                 CREATE TABLE s.a (x int);
                 CREATE TABLE a (x int);
                 CREATE TYPE s.g AS ENUM ('x');
                 CREATE TABLE g (x int);
                 CREATE TABLE e (x int);
                 CREATE INDEX d_idx ON e (x);
                 CREATE TABLE s.d_idx (x int);
                 CREATE SEQUENCE s.fseq;
                 CREATE TABLE f (x serial);
                 ALTER SEQUENCE f_x_seq RENAME TO fseq;
                 CREATE TABLE c (x serial PRIMARY KEY);
                 CREATE TYPE ct AS (x int);
                 CREATE TEMP TABLE tt (x int);
                 CREATE VIEW v AS SELECT 1 AS x;";
    for (stmt, msg) in [
        (
            "ALTER TABLE a SET SCHEMA s;",
            "relation \"a\" already exists in schema \"s\"",
        ),
        (
            "ALTER TABLE g SET SCHEMA s;",
            "type \"g\" already exists in schema \"s\"",
        ),
        (
            "ALTER TABLE e SET SCHEMA s;",
            "relation \"d_idx\" already exists in schema \"s\"",
        ),
        (
            "ALTER TABLE f SET SCHEMA s;",
            "relation \"fseq\" already exists in schema \"s\"",
        ),
        (
            "ALTER SEQUENCE c_x_seq SET SCHEMA s;",
            "cannot move an owned sequence into another schema",
        ),
        (
            "ALTER TABLE c_pkey SET SCHEMA s;",
            "cannot change schema of index \"c_pkey\"",
        ),
        ("ALTER TABLE ct SET SCHEMA s;", "\"ct\" is a composite type"),
        ("ALTER SEQUENCE v SET SCHEMA s;", "\"v\" is not a sequence"),
        (
            "ALTER TABLE tt SET SCHEMA s;",
            "cannot move objects into or out of temporary schemas",
        ),
        (
            "ALTER TABLE s.a SET SCHEMA pg_temp;",
            "cannot move objects into or out of temporary schemas",
        ),
        (
            "ALTER TABLE a SET SCHEMA pg_toast;",
            "cannot move objects into or out of TOAST schema",
        ),
        (
            "ALTER TABLE nope SET SCHEMA s;",
            "relation \"nope\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    // The index and the serial's sequence went along: their names are free
    // in public again.
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE IF EXISTS nope SET SCHEMA nope2;
             ALTER TABLE a SET SCHEMA public;
             ALTER TABLE c SET SCHEMA s;
             CREATE TABLE c_pkey (x int);
             CREATE SEQUENCE c_x_seq;
             ALTER VIEW v SET SCHEMA s;",
        ),
    ]);
    db.analyze("SELECT x FROM s.c").unwrap();
}

#[test]
fn a_missing_qualified_relation_is_named_as_pg_names_it() {
    // RangeVarGetRelidExtended: `relation "%s.%s" does not exist` joins the
    // schema and relation names as written, without quoting them — even
    // when they would need quotes as identifiers.
    let setup = "CREATE SCHEMA \"My Schema\";";
    for (stmt, msg) in [
        (
            "ALTER TABLE \"My Schema\".\"No Pe\" ADD COLUMN x int;",
            "relation \"My Schema.No Pe\" does not exist",
        ),
        (
            "TRUNCATE \"My Schema\".\"a\"\"b\";",
            "relation \"My Schema.a\"b\" does not exist",
        ),
        (
            "ALTER TABLE \"My Schema\".\"select\" ADD COLUMN x int;",
            "relation \"My Schema.select\" does not exist",
        ),
        (
            "ALTER TABLE public.nope ADD COLUMN x int;",
            "relation \"public.nope\" does not exist",
        ),
        (
            "ALTER TABLE \"No Pe\" ADD COLUMN x int;",
            "relation \"No Pe\" does not exist",
        ),
        (
            "ALTER TABLE nosch.nope ADD COLUMN x int;",
            "schema \"nosch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
}
