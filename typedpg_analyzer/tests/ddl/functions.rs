//! CREATE FUNCTION and CREATE AGGREGATE: signatures, overloading,
//! CREATE OR REPLACE, ALTER FUNCTION (rename, SET SCHEMA),
//! DROP FUNCTION — including overload-safe drops.

use crate::common::*;

// ── CREATE FUNCTION ─────────────────────────────────────────────────────────

#[test]
fn create_function_basic() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION add_one(x INT) RETURNS INT AS $$ SELECT x + 1 $$ LANGUAGE sql;",
    )]);

    let fns = snap.find_functions(None, "add_one");
    assert_eq!(fns.len(), 1);
    let f = fns[0];
    assert_eq!(f.proargtypes.len(), 1);
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    assert_eq!(f.proargtypes[0], int4_oid);
    assert_eq!(f.prorettype, int4_oid);
}

#[test]
fn create_function_defaults_let_calls_omit_trailing_args() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION f(a INT, b TEXT DEFAULT 'x', OUT o TEXT, c INT DEFAULT 1)
             AS $$ SELECT b $$ LANGUAGE sql;",
    )]);

    // OUT parameters take no default and aren't counted.
    assert_eq!(snap.find_functions(None, "f")[0].pronargdefaults, 2);
    let s = snap
        .analyze("SELECT f(1) AS one, f(1, 'y') AS two, f(1, 'y', 2) AS three")
        .unwrap();
    assert_cols(
        &s,
        vec![cn("one", text()), cn("two", text()), cn("three", text())],
    );
}

#[test]
fn create_function_variadic_records_the_element_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION f_var(VARIADIC xs INT[]) RETURNS INT
             AS $$ SELECT array_length(xs, 1) $$ LANGUAGE sql;
         CREATE FUNCTION f_var_any(VARIADIC xs anyarray) RETURNS anyelement
             AS $$ SELECT xs[1] $$ LANGUAGE sql;",
    )]);
    let provariadic = |name: &str| {
        let t = snap.find_functions(None, name)[0].provariadic.unwrap();
        snap.get_type(t).unwrap().typname.clone()
    };
    assert_eq!(provariadic("f_var"), "int4");
    assert_eq!(provariadic("f_var_any"), "anyelement");
    let s = snap.analyze("SELECT f_var(1, 2, 3) AS a").unwrap();
    assert_cols(&s, vec![cn("a", int4())]);
}

#[test]
fn create_function_variadic_non_array_is_rejected() {
    let err = try_apply(&[(
        "0001.sql",
        "CREATE FUNCTION bad(VARIADIC x INT) RETURNS INT AS $$ SELECT 1 $$ LANGUAGE sql;",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("VARIADIC parameter must be an array"),
        "got: {err}"
    );
}

// ── CREATE / DROP AGGREGATE ─────────────────────────────────────────────────

#[test]
fn create_aggregate_registers_function() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION my_sum_sfunc(int8, int4) RETURNS int8
             AS 'SELECT $1 + $2::int8' LANGUAGE SQL;
         CREATE AGGREGATE my_sum(int4) (
             SFUNC = my_sum_sfunc,
             STYPE = int8,
             INITCOND = '0'
         );",
    )]);

    let fns = snap.find_functions(None, "my_sum");
    let agg = fns
        .iter()
        .find(|f| matches!(f.prokind, ProKind::Aggregate))
        .expect("my_sum should be registered as an aggregate");
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(agg.proargtypes, vec![int4_oid]);
    assert_eq!(
        agg.prorettype, int8_oid,
        "no FINALFUNC ⇒ return type equals STYPE"
    );
}

#[test]
fn create_aggregate_with_finalfunc_uses_final_return_type() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION my_avg_sfunc(int8, int4) RETURNS int8
             AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION my_avg_finalfunc(int8) RETURNS float8
             AS 'SELECT $1::float8' LANGUAGE SQL;
         CREATE AGGREGATE my_avg(int4) (
             SFUNC = my_avg_sfunc,
             STYPE = int8,
             FINALFUNC = my_avg_finalfunc
         );",
    )]);

    let float8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "float8")
        .unwrap()
        .oid;
    let agg = snap
        .find_functions(None, "my_avg")
        .into_iter()
        .find(|f| matches!(f.prokind, ProKind::Aggregate))
        .expect("my_avg aggregate should exist");
    // The aggregate's effective return type is its finalfn's prorettype.
    // pg_aggregate stores the finalfn FK; the analyzer walks to pg_proc
    // for the type at lookup time.
    let agg_row = snap.pg_aggregate().get(&agg.oid).expect("pg_aggregate row");
    let finalfn_oid = agg_row.aggfinalfn.expect("aggfinalfn must be set");
    let finalfn_proc = snap.pg_proc().get(&finalfn_oid).expect("finalfn pg_proc");
    assert_eq!(finalfn_proc.prorettype, float8_oid);
}

#[test]
fn drop_aggregate_removes_only_aggregate() {
    // The scalar and the aggregate must take *different* argument types —
    // PG (SQLSTATE 42723) rejects two pg_proc rows sharing name + args
    // regardless of prokind.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION dup(text) RETURNS text AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION dup_sfunc(int4, int4) RETURNS int4 AS 'SELECT $1 + $2' LANGUAGE SQL;
         CREATE AGGREGATE dup(int4) (
             SFUNC = dup_sfunc,
             STYPE = int4
         );
         DROP AGGREGATE dup(int4);",
    )]);

    let fns = snap.find_functions(None, "dup");
    assert_eq!(fns.len(), 1, "scalar dup(text) should remain");
    assert_eq!(fns[0].prokind, ProKind::Function);
}

#[test]
fn drop_aggregate_missing_errors_without_if_exists() {
    let result = try_apply(&[("0001.sql", "DROP AGGREGATE nonexistent(int4);")]);
    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "aggregate nonexistent(integer) does not exist",
    );
}

#[test]
fn drop_aggregate_if_exists_no_error() {
    let _snap = build(&[("0001.sql", "DROP AGGREGATE IF EXISTS nonexistent(int4);")]);
}

// ── ALTER FUNCTION / AGGREGATE ─────────────────────────────────────────────

#[test]
fn alter_function_rename_moves_overload() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION add_one(x int) RETURNS int AS 'SELECT $1 + 1' LANGUAGE SQL;
         ALTER FUNCTION add_one(int) RENAME TO plus_one;",
    )]);

    assert!(
        snap.find_functions(None, "add_one").is_empty(),
        "old name should be gone"
    );
    let fns = snap.find_functions(None, "plus_one");
    assert_eq!(fns.len(), 1);
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    assert_eq!(fns[0].proargtypes, vec![int4_oid]);
}

#[test]
fn alter_function_set_schema_moves_it() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA utils;
         CREATE FUNCTION add_one(x int) RETURNS int AS 'SELECT $1 + 1' LANGUAGE SQL;
         ALTER FUNCTION add_one(int) SET SCHEMA utils;",
    )]);

    let fns = snap.find_functions(Some("utils"), "add_one");
    assert_eq!(fns.len(), 1);
    let utils_oid = snap.namespace_oid("utils").unwrap();
    assert_eq!(fns[0].pronamespace, utils_oid);
}

#[test]
fn alter_function_rename_with_overloads_only_moves_matching_signature() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION do_it(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION do_it(x text) RETURNS text AS 'SELECT $1' LANGUAGE SQL;
         ALTER FUNCTION do_it(int) RENAME TO do_it_int;",
    )]);

    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;

    let renamed = snap.find_functions(None, "do_it_int");
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0].proargtypes, vec![int4_oid]);

    let remaining = snap.find_functions(None, "do_it");
    assert_eq!(
        remaining.len(),
        1,
        "text overload should still be under do_it"
    );
    assert_eq!(remaining[0].proargtypes, vec![text_oid]);
}

#[test]
fn alter_aggregate_rename_only_touches_aggregate() {
    // Aggregates and regular functions share `pg_proc`'s name+args
    // namespace, so the scalar must take a different signature than the
    // aggregate (here a different argument type) — otherwise PG rejects the
    // CREATE AGGREGATE with SQLSTATE 42723.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION ag(x text) RETURNS text AS 'SELECT $1' LANGUAGE SQL;
         CREATE FUNCTION ag_sfunc(state int, val int) RETURNS int AS 'SELECT $1 + $2' LANGUAGE SQL;
         CREATE AGGREGATE ag(int) (SFUNC = ag_sfunc, STYPE = int);
         ALTER AGGREGATE ag(int) RENAME TO ag_total;",
    )]);

    // Scalar (text-arg) survives under original name.
    let scalar = snap.find_functions(None, "ag");
    assert_eq!(scalar.len(), 1);
    assert_eq!(scalar[0].prokind, ProKind::Function);

    // Aggregate (int-arg) moved.
    let moved = snap.find_functions(None, "ag_total");
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].prokind, ProKind::Aggregate);
}

// ── DROP FUNCTION vs procedure of same name ────────────────────────────────

#[test]
fn drop_function_does_not_touch_procedure_of_same_name() {
    // Function and procedure share `pg_proc`'s name+args namespace, so
    // they must take different signatures (PG SQLSTATE 42723 otherwise).
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION f(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE PROCEDURE f(x text) LANGUAGE SQL AS $$ SELECT 1 $$;
         DROP FUNCTION f(int);",
    )]);

    // find_functions filters out procedures, so look at pg_proc directly.
    let public_oid = snap.namespace_oid("public").unwrap();
    let procs: Vec<&PgProc> = snap
        .pg_proc()
        .values()
        .filter(|p| p.pronamespace == public_oid && p.proname == "f")
        .collect();
    assert_eq!(procs.len(), 1, "procedure must survive DROP FUNCTION");
    assert_eq!(procs[0].prokind, ProKind::Procedure);
}

// ── CREATE OR REPLACE / overloading ────────────────────────────────────────

#[test]
fn create_or_replace_function_updates_existing_body() {
    // CREATE OR REPLACE only swaps the body — return type is fixed.
    // Changing the return type would be SQLSTATE 42P13
    // ("cannot change return type of existing function").
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
         CREATE OR REPLACE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x + 1 $$ LANGUAGE sql;",
    )]);

    let fns = snap.find_functions(None, "foo");
    assert_eq!(fns.len(), 1, "should have exactly 1 overload");
    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    assert_eq!(fns[0].prorettype, int4_oid);
}

#[test]
fn create_or_replace_function_with_different_return_type_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
             CREATE OR REPLACE FUNCTION foo(x INT) RETURNS BIGINT AS $$ SELECT x::bigint $$ LANGUAGE sql;",
        )]),
        DdlError::DuplicateObject(_),
        "cannot change return type of existing function",
    );
}

#[test]
fn create_function_overloading_distinct_arg_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
         CREATE FUNCTION foo(x TEXT) RETURNS TEXT AS $$ SELECT x $$ LANGUAGE sql;",
    )]);

    let fns = snap.find_functions(None, "foo");
    assert_eq!(fns.len(), 2, "should have 2 overloads");
}

#[test]
fn create_function_duplicate_signature_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x $$ LANGUAGE sql;
         CREATE FUNCTION foo(x INT) RETURNS INT AS $$ SELECT x + 1 $$ LANGUAGE sql;",
    )]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "function \"foo\" already exists with same argument types"
    );
}

// ── Full blog schema: multi-migration integration test ─────────────────────

#[test]
fn full_blog_schema() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE users (
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                name TEXT NOT NULL,
                email TEXT NOT NULL UNIQUE,
                age INT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );",
        ),
        (
            "0002.sql",
            "CREATE TYPE post_status AS ENUM ('draft', 'published', 'archived');
             CREATE TABLE posts (
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                user_id BIGINT NOT NULL REFERENCES users(id),
                title TEXT NOT NULL,
                body TEXT,
                status post_status NOT NULL DEFAULT 'draft',
                published_at TIMESTAMPTZ,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );
             CREATE INDEX idx_posts_user_id ON posts (user_id);",
        ),
        (
            "0003.sql",
            "ALTER TABLE users ADD COLUMN bio TEXT;
             ALTER TYPE post_status ADD VALUE 'deleted' AFTER 'archived';",
        ),
    ]);

    // Users table has 6 columns now (id, name, email, age, created_at, bio).
    let users = snap.resolve_table(None, "users").unwrap();
    let user_attrs = snap.attributes_of(users.oid);
    assert_eq!(user_attrs.len(), 6);
    assert_eq!(user_attrs[5].attname, "bio");
    assert!(!user_attrs[5].attnotnull);

    // Posts table.
    let posts = snap.resolve_table(None, "posts").unwrap();
    assert_eq!(snap.attributes_of(posts.oid).len(), 7);

    // post_status enum has 4 values.
    let ps = snap.resolve_type_by_name(None, "post_status").unwrap();
    assert_eq!(ps.typtype, TypType::Enum);
    let labels = snap.enum_labels_of(ps.oid);
    assert_eq!(labels, vec!["draft", "published", "archived", "deleted"]);
}
