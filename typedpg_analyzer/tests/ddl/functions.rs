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
    // `f(1, 'y', 2)` is its body, `b`: 'y'. A call leaving parameters to
    // their defaults isn't read as its body.
    assert_cols(
        &s,
        vec![cn("one", text()), cn("two", text()), c("three", text())],
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
        DdlError::TypeNotFound(_),
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

#[test]
fn create_function_with_unknown_types_is_rejected() {
    // PG 18: the parameter-list message leaves the name unquoted
    // (interpret_function_parameter_list), the return type one quotes it.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE FUNCTION f(a nosuchtype) RETURNS int LANGUAGE sql AS 'select 1';",
        )]),
        DdlError::TypeNotFound(_),
        "type nosuchtype does not exist",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE FUNCTION f() RETURNS nosuchtype LANGUAGE sql AS 'select 1';",
        )]),
        DdlError::TypeNotFound(_),
        "type \"nosuchtype\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE FUNCTION f() RETURNS SETOF nosuchtype LANGUAGE sql AS 'select 1';",
        )]),
        DdlError::TypeNotFound(_),
        "type \"nosuchtype\" does not exist",
    );
}

#[test]
fn create_function_pct_type_resolves_to_column_type() {
    // PG 18: NOTICE type reference t.a%TYPE converted to bigint; f(1) is bigint.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a bigint);
         CREATE FUNCTION f(x t.a%TYPE) RETURNS t.a%TYPE LANGUAGE sql AS 'select x';",
    )]);
    let info = db.analyze("SELECT f(1)").unwrap();
    // Its body, `x`: 1.
    assert_cols(&info, vec![c("f", int8())]);
}

#[test]
fn create_function_pct_type_errors() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t3 (a int);
             CREATE FUNCTION g2(x t3.zz%TYPE) RETURNS int LANGUAGE sql AS 'select 1';",
        )]),
        DdlError::Parse(_),
        "column \"zz\" of relation \"t3\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE FUNCTION g3(x nosuchrel.zz%TYPE) RETURNS int LANGUAGE sql AS 'select 1';",
        )]),
        DdlError::TableNotFound(_),
        "relation \"nosuchrel\" does not exist",
    );
}

// ── SQL function bodies (fmgr_sql_validator) ────────────────────────────────

#[test]
fn sql_function_bodies_are_validated() {
    for (sql, msg) in [
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'select a from nosuch';",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$ select 'a'::text $$;",
            "return type mismatch in function declared to return integer",
        ),
        (
            "CREATE FUNCTION f(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT nosuchcol; END;",
            "column \"nosuchcol\" does not exist",
        ),
        (
            "CREATE FUNCTION f5() RETURNS int LANGUAGE sql AS 'select 1, 2';",
            "return type mismatch in function declared to return integer",
        ),
        (
            "CREATE TABLE t (a int, b int);
             CREATE FUNCTION f7(x int) RETURNS int LANGUAGE sql AS 'insert into t values (x)';",
            "return type mismatch in function declared to return integer",
        ),
        (
            "CREATE TABLE t (a int, b int);
             CREATE FUNCTION f11() RETURNS int LANGUAGE sql AS 'select 1 from t where nosuchcol = 1';",
            "column \"nosuchcol\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
}

#[test]
fn valid_sql_function_bodies_are_accepted() {
    // PG 18 accepts all of these: parameters by name and number, an
    // assignment-castable result, DML, recursion, and bodies left unchecked
    // under check_function_bodies = false.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int);
         CREATE FUNCTION f2(x int) RETURNS int LANGUAGE sql RETURN x + 1;
         CREATE FUNCTION f3(x int, y text) RETURNS text LANGUAGE sql AS 'select y || $1::text';
         CREATE FUNCTION f4() RETURNS int LANGUAGE sql AS 'select 1::bigint';
         CREATE FUNCTION f6(x int) RETURNS void LANGUAGE sql AS 'insert into t values (x)';
         CREATE FUNCTION f8(a int) RETURNS int LANGUAGE sql AS 'select a from t';
         CREATE FUNCTION f9(n int) RETURNS int LANGUAGE sql
             AS 'select case when n <= 0 then 0 else f9(n - 1) end';
         SET check_function_bodies = false;
         CREATE FUNCTION f10() RETURNS int LANGUAGE sql AS 'select a from nosuch';
         RESET check_function_bodies;",
    )]);
    // Its body, `x + 1`, of 1.
    assert_cols(&db.analyze("SELECT f2(1)").unwrap(), vec![c("f2", int4())]);
}

#[test]
fn sql_function_parameter_names_resolve_after_columns() {
    // PG's sql_fn_post_column_ref: a column wins over a same-named
    // parameter, `fname.param` names the parameter, and a body whose
    // parameters shadow columns is still validated.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL);
         CREATE TYPE pair AS (a int, b text);
         CREATE FUNCTION c1(id int) RETURNS text LANGUAGE sql AS $$ SELECT name FROM t WHERE id = id $$;
         CREATE FUNCTION c2(id text) RETURNS int LANGUAGE sql AS $$ SELECT id FROM t $$;
         CREATE FUNCTION c4(name int) RETURNS int LANGUAGE sql AS $$ SELECT c4.name + 1 FROM t $$;
         CREATE FUNCTION c6(p pair) RETURNS text LANGUAGE sql AS $$ SELECT p.b $$;
         CREATE FUNCTION c7(p pair) RETURNS text LANGUAGE sql AS $$ SELECT c7.p.b $$;",
    )]);
    for (sql, msg) in [
        (
            "CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL);
             CREATE FUNCTION c5(x int) RETURNS int LANGUAGE sql AS $$ SELECT x FROM t WHERE nope = x $$;",
            "column \"nope\" does not exist",
        ),
        // `id` is t's integer column, not the text parameter.
        (
            "CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL);
             CREATE FUNCTION c8(id text) RETURNS int LANGUAGE sql AS $$ SELECT 1 FROM t WHERE id = 'a' || 'b'::text $$;",
            "operator does not exist: integer = text",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
}

#[test]
fn sql_function_result_rows_are_checked_like_check_sql_fn_retval() {
    let setup = "CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL);
                 CREATE TYPE pair AS (a int, b text);";
    // Accepted by PG 18: assignment coercions per column, a lone column of
    // the composite type, a table's row, a bare record.
    build_db(&[(
        "0001.sql",
        &format!(
            "{setup}
             CREATE FUNCTION r2() RETURNS pair LANGUAGE sql AS $$ SELECT 1, 'x'::text $$;
             CREATE FUNCTION r6() RETURNS pair LANGUAGE sql AS $$ SELECT ROW(1,'x')::pair $$;
             CREATE FUNCTION r7(OUT a int, OUT b text) LANGUAGE sql AS $$ SELECT 1, 2 $$;
             CREATE FUNCTION r9() RETURNS t LANGUAGE sql AS $$ SELECT * FROM t $$;
             CREATE FUNCTION r11() RETURNS TABLE (a int, b text) LANGUAGE sql AS $$ SELECT 1, 'x' $$;
             CREATE FUNCTION r13() RETURNS record LANGUAGE sql AS $$ SELECT 1, 2 $$;
             CREATE FUNCTION r14() RETURNS pair LANGUAGE sql AS $$ SELECT 1::int8, 'x' $$;
             CREATE FUNCTION r15() RETURNS pair LANGUAGE sql AS $$ SELECT 1.5, 'x' $$;"
        ),
    )]);
    for (function, msg) in [
        (
            "CREATE FUNCTION r1() RETURNS SETOF int LANGUAGE sql AS $$ SELECT name FROM t $$;",
            "return type mismatch in function declared to return integer (Actual return type is text.)",
        ),
        (
            "CREATE FUNCTION r3() RETURNS pair LANGUAGE sql AS $$ SELECT 1 $$;",
            "return type mismatch in function declared to return pair (Final statement returns too few columns.)",
        ),
        (
            "CREATE FUNCTION r4() RETURNS pair LANGUAGE sql AS $$ SELECT 1, 2, 3 $$;",
            "return type mismatch in function declared to return pair (Final statement returns too many columns.)",
        ),
        (
            "CREATE FUNCTION r5() RETURNS pair LANGUAGE sql AS $$ SELECT 'x'::text, 1 $$;",
            "return type mismatch in function declared to return pair (Final statement returns text instead of integer at column 1.)",
        ),
        (
            "CREATE FUNCTION r8(OUT a int, OUT b text) LANGUAGE sql AS $$ SELECT 1 $$;",
            "return type mismatch in function declared to return record (Final statement returns too few columns.)",
        ),
        (
            "CREATE FUNCTION r10() RETURNS SETOF t LANGUAGE sql AS $$ SELECT id FROM t $$;",
            "return type mismatch in function declared to return t (Final statement returns too few columns.)",
        ),
        (
            "CREATE FUNCTION r12() RETURNS TABLE (a int, b int) LANGUAGE sql AS $$ SELECT 1, 'x'::text $$;",
            "return type mismatch in function declared to return record (Final statement returns text instead of integer at column 2.)",
        ),
    ] {
        let sql = format!("{setup}\n{function}");
        let err = try_apply(&[("0001.sql", &sql)]).expect_err(function);
        assert!(err.to_string().starts_with(msg), "{function}\n  got: {err}");
    }
}

#[test]
fn sql_function_bodies_report_every_analysis_error() {
    for (sql, msg) in [
        (
            "CREATE FUNCTION e1(x int) RETURNS int LANGUAGE sql AS $$ SELECT nofunc(x) $$;",
            "function nofunc(integer) does not exist",
        ),
        (
            "CREATE FUNCTION e2(x int) RETURNS int LANGUAGE sql AS $$ SELECT x + 'a'::text $$;",
            "operator does not exist: integer + text",
        ),
        (
            "CREATE TABLE t (id int);
             CREATE FUNCTION e3() RETURNS int LANGUAGE sql AS $$ SELECT count(*) FROM t WHERE count(*) > 1 $$;",
            "aggregate functions are not allowed in WHERE",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
}

// ── ALTER FUNCTION, SQL-function inlining in index expressions ──────────────

#[test]
fn alter_function_volatility_is_applied() {
    // PG 18: after ALTER FUNCTION ... IMMUTABLE the index is accepted; after
    // ... VOLATILE it's rejected again.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int);
         CREATE FUNCTION f(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$;
         ALTER FUNCTION f(int) IMMUTABLE;
         CREATE INDEX ON t (f(a));
         ALTER FUNCTION f STABLE;",
    )]);
    let err = try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (a int);
         CREATE FUNCTION f(a int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN a; END $$;
         ALTER FUNCTION f(int) VOLATILE;
         CREATE INDEX ON t ((f(a) + 1));",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("functions in index expression must be marked IMMUTABLE"),
        "{err}"
    );
    assert_ddl_err!(
        try_apply(&[("0001.sql", "ALTER FUNCTION nosuch(int) IMMUTABLE;")]),
        DdlError::TypeNotFound(_),
        "function nosuch(integer) does not exist",
    );
}

#[test]
fn inlinable_sql_functions_are_judged_by_their_body() {
    // PG 18: `select a` inlines to a plain column (accepted even though the
    // function is VOLATILE); `select random()` stays volatile.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int);
         CREATE FUNCTION g(a int) RETURNS int LANGUAGE sql AS 'select a';
         CREATE INDEX ON t (g(a));",
    )]);
    let err = try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (a int);
         CREATE FUNCTION h(a int) RETURNS float8 LANGUAGE sql AS 'select random()';
         CREATE INDEX ON t (h(a));",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("functions in index expression must be marked IMMUTABLE"),
        "{err}"
    );
}

// ── Parameter defaults (interpret_function_parameter_list / ParseFuncOrColumn)

#[test]
fn polymorphic_parameter_defaults_take_part_in_resolution() {
    // PG 18: the default of a polymorphic parameter keeps its own type,
    // and an omitted parameter's default type counts when the call's
    // polymorphic types are resolved.
    let db = build_db(&[(
        "0001.sql",
        "CREATE FUNCTION pd(a anyelement, b anyelement DEFAULT 1) RETURNS anyelement
             LANGUAGE sql AS 'select a';
         CREATE FUNCTION pd2(a anyelement DEFAULT 1::int8) RETURNS anyelement
             LANGUAGE sql AS 'select a';
         CREATE FUNCTION pd3(a int, b anyelement DEFAULT 'x'::text) RETURNS anyelement
             LANGUAGE sql AS 'select b';
         CREATE FUNCTION pd4(a anyelement DEFAULT NULL) RETURNS anyelement
             LANGUAGE sql AS 'select a';
         CREATE FUNCTION pd5(a anycompatible, b anycompatible DEFAULT 1) RETURNS anycompatible
             LANGUAGE sql AS 'select a';",
    )]);
    for (sql, msg) in [
        (
            "SELECT pd(1.5)",
            "arguments declared \"anyelement\" are not all alike",
        ),
        (
            "SELECT pd('x'::text)",
            "arguments declared \"anyelement\" are not all alike",
        ),
        (
            "SELECT pd4()",
            "could not determine polymorphic type because input has type unknown",
        ),
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    assert_cols(&db.analyze("SELECT pd(2)").unwrap(), vec![cn("pd", int4())]);
    assert_cols(
        &db.analyze("SELECT pd2()").unwrap(),
        vec![cn("pd2", int8())],
    );
    // Its body, `a`: 1.
    assert_cols(
        &db.analyze("SELECT pd2(1)").unwrap(),
        vec![c("pd2", int4())],
    );
    assert_cols(
        &db.analyze("SELECT pd3(1)").unwrap(),
        vec![cn("pd3", text())],
    );
    assert_cols(
        &db.analyze("SELECT pd5(1.5)").unwrap(),
        vec![cn("pd5", numeric())],
    );
}

#[test]
fn parameter_defaults_are_checked() {
    for (sql, msg) in [
        (
            "CREATE FUNCTION f1(a int DEFAULT 'x') RETURNS int LANGUAGE sql AS 'select 1';",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "CREATE FUNCTION f2(a int DEFAULT now()) RETURNS int LANGUAGE sql AS 'select 1';",
            "argument of DEFAULT must be type integer, not type timestamp with time zone",
        ),
        (
            "CREATE FUNCTION f4(a int DEFAULT 1, b int) RETURNS int LANGUAGE sql AS 'select 1';",
            "input parameters after one with a default value must also have defaults",
        ),
        (
            "CREATE FUNCTION f5(a int, b int DEFAULT a) RETURNS int LANGUAGE sql AS 'select 1';",
            "column \"a\" does not exist",
        ),
        (
            "CREATE FUNCTION f6(a int DEFAULT (select 1)) RETURNS int LANGUAGE sql AS 'select 1';",
            "cannot use subquery in DEFAULT expression",
        ),
        (
            "CREATE FUNCTION f9(OUT a int DEFAULT 1) RETURNS int LANGUAGE sql AS 'select 1';",
            "only input parameters can have default values",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    // Assignment-castable defaults are fine.
    build_db(&[(
        "0001.sql",
        "CREATE FUNCTION f3(a int DEFAULT 1.5) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE FUNCTION f7(a text DEFAULT 5) RETURNS int LANGUAGE sql AS 'select 1';",
    )]);
}

// ── ProcedureCreate / LookupFuncWithArgs rules ─────────────────────────────

#[test]
fn create_function_signature_rules() {
    for (sql, msg) in [
        (
            "CREATE FUNCTION bad(int) RETURNS anyelement LANGUAGE sql AS 'select 1';",
            "cannot determine result data type",
        ),
        (
            "CREATE FUNCTION bad2(anyelement) RETURNS anyrange LANGUAGE sql AS 'select null';",
            "cannot determine result data type",
        ),
        (
            "CREATE FUNCTION bad3(anyelement) RETURNS anycompatiblearray LANGUAGE sql AS 'select null';",
            "cannot determine result data type",
        ),
        (
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE OR REPLACE FUNCTION f(b int) RETURNS int LANGUAGE sql AS 'select 1';",
            "cannot change name of input parameter \"a\"",
        ),
        (
            "CREATE FUNCTION f1(a int DEFAULT 1) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE OR REPLACE FUNCTION f1(a int) RETURNS int LANGUAGE sql AS 'select 1';",
            "cannot remove parameter defaults from existing function",
        ),
        (
            "CREATE FUNCTION f2(a int) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE OR REPLACE FUNCTION f2(a int) RETURNS SETOF int LANGUAGE sql AS 'select 1';",
            "cannot change return type of existing function",
        ),
        (
            "CREATE FUNCTION f4(a int) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE OR REPLACE PROCEDURE f4(a int) LANGUAGE sql AS 'select 1';",
            "cannot change routine kind",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    // Allowed replacements: new body, a newly named parameter, an added
    // default.
    build_db(&[(
        "0001.sql",
        "CREATE FUNCTION g(int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE OR REPLACE FUNCTION g(a int DEFAULT 2) RETURNS int LANGUAGE sql AS 'select 2';
         CREATE FUNCTION ok(anyarray) RETURNS anyelement LANGUAGE sql AS 'select $1[1]';
         CREATE FUNCTION ok2(anycompatible, anycompatible) RETURNS anycompatiblearray
             LANGUAGE sql AS 'select array[$1, $2]';",
    )]);
}

#[test]
fn drop_function_lookup_follows_lookup_func_with_args() {
    for (sql, msg) in [
        (
            "CREATE FUNCTION f3(int) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE FUNCTION f3(text) RETURNS int LANGUAGE sql AS 'select 1';
             DROP FUNCTION f3;",
            "function name \"f3\" is not unique",
        ),
        (
            "DROP FUNCTION nosuch(int);",
            "function nosuch(integer) does not exist",
        ),
        (
            "CREATE PROCEDURE p(int) LANGUAGE sql AS 'select 1'; DROP FUNCTION p(int);",
            "p(integer) is not a function",
        ),
        (
            "CREATE FUNCTION q(int) RETURNS int LANGUAGE sql AS 'select 1'; DROP PROCEDURE q(int);",
            "q(integer) is not a procedure",
        ),
        (
            "DROP FUNCTION f(nosuchtype);",
            "type \"nosuchtype\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
         DROP FUNCTION f;
         DROP FUNCTION IF EXISTS f(nosuchtype);
         DROP FUNCTION IF EXISTS nosuch(int);",
    )]);
}

#[test]
fn plpgsql_function_bodies_are_compiled() {
    // PG 18 (plpgsql_validator): syntax errors, unknown declared types.
    for (sql, msg) in [
        (
            "CREATE FUNCTION pf() RETURNS int LANGUAGE plpgsql AS 'begin retrun 1; end';",
            "syntax error at or near \"retrun\"",
        ),
        (
            "CREATE FUNCTION pf2() RETURNS int LANGUAGE plpgsql AS 'declare x nosuchtype; begin return 1; end';",
            "type \"nosuchtype\" does not exist",
        ),
        (
            "CREATE FUNCTION pf4() RETURNS int LANGUAGE plpgsql AS 'begin select 1 +; end';",
            "syntax error at end of input",
        ),
        (
            "CREATE FUNCTION pf5() RETURNS int LANGUAGE plpgsql AS 'begin return 1 end';",
            "syntax error at end of input",
        ),
        (
            "CREATE FUNCTION pf6() RETURNS int LANGUAGE plpgsql AS 'declare r nosuch%ROWTYPE; begin return 1; end';",
            "relation \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    // Bodies PG accepts (a missing relation inside a query is only found at
    // run time), and anything under check_function_bodies = false.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a bigint);
         CREATE FUNCTION ok1() RETURNS int LANGUAGE plpgsql
             AS 'declare x int; y t.a%TYPE; r t%ROWTYPE; rr record; begin return (select a from nosuch); end';
         SET check_function_bodies = false;
         CREATE FUNCTION ok2() RETURNS int LANGUAGE plpgsql AS 'begin retrun 1; end';",
    )]);
}

#[test]
fn plpgsql_bodies_compile_against_the_migration_catalog() {
    // PL/pgSQL's compiler resolves types, schemas, %TYPE and %ROWTYPE
    // against the catalog the migrations built. Expectations from PG 18.
    let setup = "CREATE SCHEMA app;
                 CREATE TYPE app.mood AS ENUM ('a');
                 CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);
                 CREATE TYPE shelly;";
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE FUNCTION p1(m app.mood, VARIADIC xs int4[]) RETURNS app.mood LANGUAGE plpgsql
                 AS $$ DECLARE ms app.mood[]; r users%ROWTYPE; n users.name%TYPE; BEGIN RETURN m; END $$;
             CREATE FUNCTION p7(x users.id%TYPE) RETURNS int LANGUAGE plpgsql
                 AS $$ BEGIN RETURN x; END $$;
             CREATE FUNCTION p8() RETURNS trigger LANGUAGE plpgsql
                 AS $$ BEGIN NEW.name := upper(NEW.name); RETURN NEW; END $$;
             DO $$ DECLARE m app.mood; BEGIN NULL; END $$;",
        ),
    ]);
    for (function, message) in [
        (
            "CREATE FUNCTION p2() RETURNS int LANGUAGE plpgsql AS $$ DECLARE x shelly; BEGIN RETURN 1; END $$;",
            "type \"shelly\" is only a shell",
        ),
        (
            "CREATE FUNCTION p3() RETURNS int LANGUAGE plpgsql AS $$ DECLARE x app.nosuch; BEGIN RETURN 1; END $$;",
            "type \"app.nosuch\" does not exist",
        ),
        (
            "CREATE FUNCTION p4() RETURNS int LANGUAGE plpgsql AS $$ DECLARE x nope.t; BEGIN RETURN 1; END $$;",
            "schema \"nope\" does not exist",
        ),
        (
            "CREATE FUNCTION p5() RETURNS int LANGUAGE plpgsql AS $$ DECLARE r app.users%ROWTYPE; BEGIN RETURN 1; END $$;",
            "relation \"app.users\" does not exist",
        ),
        (
            "CREATE FUNCTION p6() RETURNS int LANGUAGE plpgsql AS $$ DECLARE n users.nope%TYPE; BEGIN RETURN 1; END $$;",
            "column \"nope\" of relation \"users\" does not exist",
        ),
        (
            "CREATE FUNCTION p9() RETURNS int LANGUAGE plpgsql AS $$ DECLARE r record[]; BEGIN RETURN 1; END $$;",
            "variable \"r\" has pseudo-type record[]",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", function)]).expect_err(function);
        assert!(
            err.to_string().starts_with(message),
            "{function}\n  got: {err}"
        );
    }
}

#[test]
fn sql_function_parameters_are_typed_params() {
    // A SQL function's $n are Params of the argument types (not casts), and
    // a $n beyond the arguments is PG's `there is no parameter $n`.
    build_db(&[(
        "0001.sql",
        "CREATE FUNCTION q3(a int, b text) RETURNS text LANGUAGE sql AS $$ SELECT $2 || 'x' WHERE $1 > 0 $$;
         CREATE FUNCTION q4(a int) RETURNS int LANGUAGE sql RETURN $1 + 1;
         CREATE FUNCTION q5(a int) RETURNS text LANGUAGE sql AS $$ SELECT $1 || 'x' $$;",
    )]);
    for (function, message) in [
        (
            "CREATE FUNCTION q1(a int) RETURNS int LANGUAGE sql AS $$ SELECT $2 $$;",
            "there is no parameter $2",
        ),
        (
            "CREATE FUNCTION q2(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT $2; END;",
            "there is no parameter $2",
        ),
        (
            "CREATE FUNCTION q6(a text) RETURNS bool LANGUAGE sql AS $$ SELECT $1 = 1 $$;",
            "operator does not exist: text = integer",
        ),
    ] {
        let err = try_apply(&[("0001.sql", function)]).expect_err(function);
        assert!(
            err.to_string().starts_with(message),
            "{function}\n  got: {err}"
        );
    }
}

#[test]
fn inline_body_in_another_language_is_rejected_without_aborting() {
    // libpg_query's PL/pgSQL entry point used to fail a C assert (and abort
    // the process) on a routine with no string body.
    assert_ddl_rejections(&[
        (
            "",
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE plpgsql RETURN a + 1;",
            "inline SQL function body only valid for language SQL",
        ),
        (
            "",
            "CREATE PROCEDURE p() LANGUAGE plpgsql BEGIN ATOMIC SELECT 1; END;",
            "inline SQL function body only valid for language SQL",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql;",
            "no function body specified",
        ),
        (
            "",
            "CREATE FUNCTION f(a anyelement) RETURNS int LANGUAGE sql RETURN 1;",
            "SQL function with unquoted function body cannot have polymorphic arguments",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int AS 'select 1';",
            "no language specified",
        ),
    ]);
    // Without LANGUAGE an inline body is SQL.
    build(&[("0001.sql", "CREATE FUNCTION f() RETURNS int RETURN 1;")]);
}

#[test]
fn create_function_validates_options_and_parameters() {
    let f = "CREATE FUNCTION f13(int) RETURNS int LANGUAGE sql AS 'select 1';";
    assert_ddl_rejections(&[
        (
            "",
            "CREATE FUNCTION f(a int, a int) RETURNS int LANGUAGE sql AS 'select 1';",
            "parameter name \"a\" used more than once",
        ),
        (
            "",
            "CREATE FUNCTION f(INOUT a int, OUT a int) LANGUAGE sql AS 'select 1, 1';",
            "parameter name \"a\" used more than once",
        ),
        (
            "",
            "CREATE FUNCTION f(VARIADIC a int[], b int) RETURNS int LANGUAGE sql AS 'select 1';",
            "VARIADIC parameter must be the last input parameter",
        ),
        (
            "",
            "CREATE PROCEDURE p(VARIADIC a int[], OUT b int) LANGUAGE sql AS 'select 1';",
            "VARIADIC parameter must be the last parameter",
        ),
        (
            "",
            "CREATE PROCEDURE p(a int DEFAULT 1, OUT b int) LANGUAGE sql AS 'select 1';",
            "procedure OUT parameters cannot appear after one with a default value",
        ),
        (
            "",
            "CREATE FUNCTION f(setof int) RETURNS int LANGUAGE sql AS 'select 1';",
            "functions cannot accept set arguments",
        ),
        (
            "",
            "CREATE FUNCTION f(OUT a int) RETURNS text LANGUAGE sql AS 'select 1';",
            "function result type must be integer because of OUT parameters",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql COST 0 AS 'select 1';",
            "COST must be positive",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql ROWS 10 AS 'select 1';",
            "ROWS is not applicable when function does not return a set",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS SETOF int LANGUAGE sql ROWS 0 AS 'select 1';",
            "ROWS must be positive",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql IMMUTABLE VOLATILE AS 'select 1';",
            "conflicting or redundant options",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'select 1' AS 'select 2';",
            "conflicting or redundant options",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql PARALLEL bogus AS 'select 1';",
            "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE internal AS 'nosuchfn';",
            "there is no built-in function named \"nosuchfn\"",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql TRANSFORM FOR TYPE int AS 'select 1';",
            "transform for type integer language \"sql\" does not exist",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql SUPPORT nosuch AS 'select 1';",
            "function nosuch(internal) does not exist",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql SET work_mem = 'abc' AS 'select 1';",
            "invalid value for parameter \"work_mem\": \"abc\"",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql SET max_connections = 5 AS 'select 1';",
            "parameter \"max_connections\" cannot be changed without restarting the server",
        ),
        (
            "",
            "CREATE PROCEDURE p() LANGUAGE sql STRICT AS 'select 1';",
            "invalid attribute in procedure definition",
        ),
        (
            "CREATE PROCEDURE p() LANGUAGE sql AS 'select 1';",
            "ALTER PROCEDURE p() IMMUTABLE;",
            "invalid attribute in procedure definition",
        ),
        (
            "",
            "CREATE FUNCTION f() RETURNS int AS '' LANGUAGE sql;",
            "return type mismatch in function declared to return integer",
        ),
        (
            // PostgreSQL 18 analyzes every statement of the body; the
            // CREATE TABLE only runs when the function does.
            "",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'create table x(a int); select a from x';",
            "relation \"x\" does not exist",
        ),
        (
            f,
            "ALTER FUNCTION f13(int) SET nonexistent_guc = 1;",
            "unrecognized configuration parameter \"nonexistent_guc\"",
        ),
        (
            f,
            "ALTER FUNCTION f13(int) ROWS 10;",
            "ROWS is not applicable when function does not return a set",
        ),
        (
            "CREATE FUNCTION f13(int) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE FUNCTION g(int) RETURNS int LANGUAGE sql AS 'select 1';",
            "ALTER FUNCTION f13 RENAME TO g;",
            "function g(integer) already exists in schema \"public\"",
        ),
    ]);
    build(&[(
        "0001.sql",
        "CREATE FUNCTION f1() RETURNS int LANGUAGE sql SET a.b = 5 AS 'select 1';
         CREATE FUNCTION f2() RETURNS int LANGUAGE internal AS 'int4pl';
         CREATE FUNCTION f3(a int, OUT a int) LANGUAGE sql AS 'select 1';",
    )]);
}

#[test]
fn create_or_replace_function_keeps_out_names_and_routine_kind() {
    assert_ddl_rejections(&[
        (
            "CREATE FUNCTION h(OUT a int, OUT b int) LANGUAGE sql AS 'select 1, 2';",
            "CREATE OR REPLACE FUNCTION h(OUT a int, OUT c int) LANGUAGE sql AS 'select 1, 2';",
            "cannot change return type of existing function",
        ),
        (
            "CREATE FUNCTION w(int) RETURNS int LANGUAGE sql AS 'select 1';",
            "CREATE OR REPLACE FUNCTION w(int) RETURNS int WINDOW LANGUAGE internal \
             AS 'window_row_number';",
            "cannot change routine kind",
        ),
        (
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS 'select 1';",
            "CREATE OR REPLACE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';",
            "cannot change name of input parameter \"a\"",
        ),
    ]);
}

#[test]
fn alter_function_without_arguments_needs_a_unique_name() {
    let db = build(&[(
        "0001.sql",
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS 'select 1';
         ALTER FUNCTION f RENAME TO g;
         ALTER ROUTINE g IMMUTABLE;",
    )]);
    assert_eq!(db.find_functions(None, "g").len(), 1);
    assert_ddl_rejections(&[(
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE FUNCTION f(text) RETURNS int LANGUAGE sql AS 'select 1';",
        "ALTER FUNCTION f RENAME TO g;",
        "function name \"f\" is not unique",
    )]);
}

#[test]
fn create_aggregate_resolves_its_final_function_like_a_call() {
    // FINALFUNC is looked up with the state type as argument, and a
    // polymorphic final function's result is resolved from it.
    let db = build(&[(
        "0001.sql",
        "CREATE FUNCTION ff(bigint) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE FUNCTION ff(int) RETURNS text LANGUAGE sql AS 'select ''x''';
         CREATE AGGREGATE a1(int) (sfunc = int4pl, stype = int, finalfunc = ff);
         CREATE FUNCTION fp(anyelement) RETURNS anyelement LANGUAGE sql AS 'select $1';
         CREATE AGGREGATE a2(int) (sfunc = int4pl, stype = int, finalfunc = fp);
         CREATE AGGREGATE mysum(int) (sfunc = int4pl, stype = int);
         CREATE OR REPLACE AGGREGATE mysum(int) (sfunc = int4larger, stype = int);
         CREATE AGGREGATE a3 (basetype = int, sfunc = int4pl, stype = int);",
    )]);
    let q = db
        .analyze("SELECT a1(1) AS x, a2(1) AS y, mysum(1) AS z, a3(1) AS w")
        .unwrap();
    assert_eq!(q.columns[0].pg_type, text());
    assert_eq!(q.columns[1].pg_type, int4());
    assert_eq!(q.columns[2].pg_type, int4());
    assert_eq!(q.columns[3].pg_type, int4());
}

#[test]
fn create_aggregate_validates_its_definition() {
    let agg = |def: &str| format!("CREATE AGGREGATE a(int) ({def});");
    let cases: Vec<(String, &str)> = vec![
        (
            "CREATE AGGREGATE a(nosuchtype) (sfunc = int4pl, stype = int);".into(),
            "type nosuchtype does not exist",
        ),
        (
            agg("sfunc = nosuchfn, stype = int"),
            "function nosuchfn(integer, integer) does not exist",
        ),
        (
            agg("sfunc = int4pl, stype = bigint"),
            "function int4pl(bigint, integer) does not exist",
        ),
        (agg("sfunc = int4pl"), "aggregate stype must be specified"),
        (
            agg("sfunc = int4pl, stype = int, initcond = 'x'"),
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            agg("sfunc = int4pl, stype = int, finalfunc = float8abs"),
            "function float8abs(double precision) requires run-time type coercion",
        ),
        (
            agg("sfunc = int4pl, stype = int, combinefunc = int8pl"),
            "function int8pl(bigint, bigint) requires run-time type coercion",
        ),
        (
            agg("sfunc = int4pl, stype = int, msfunc = int4pl, mstype = int"),
            "aggregate minvfunc must be specified when mstype is specified",
        ),
        (
            agg("sfunc = int4pl, stype = int, mstype = int"),
            "aggregate msfunc must be specified when mstype is specified",
        ),
        (
            agg("sfunc = int4pl, stype = int, serialfunc = int4send"),
            "must specify both or neither of serialization and deserialization functions",
        ),
        (
            agg("sfunc = int4pl, stype = int, hypothetical"),
            "only ordered-set aggregates can be hypothetical",
        ),
        (
            agg("sfunc = int4pl, stype = int, finalfunc_modify = bogus"),
            "parameter \"finalfunc_modify\" must be READ_ONLY, SHAREABLE, or READ_WRITE",
        ),
        (
            agg("sfunc = int4pl, stype = int, parallel = bogus"),
            "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE",
        ),
        (
            "CREATE AGGREGATE a(*) (sfunc = int4pl, stype = int);".into(),
            "function int4pl(integer) does not exist",
        ),
        (
            agg("sfunc = int4pl, stype = nosuchtype"),
            "type \"nosuchtype\" does not exist",
        ),
        (
            agg("sfunc = int4pl, stype = int, finalfunc = nosuchfn"),
            "function nosuchfn(integer) does not exist",
        ),
    ];
    let cases: Vec<(&str, &str, &str)> = cases.iter().map(|(s, m)| ("", s.as_str(), *m)).collect();
    assert_ddl_rejections(&cases);
    assert_ddl_rejections(&[
        (
            "CREATE FUNCTION fs(int) RETURNS SETOF int LANGUAGE sql AS 'select 1';",
            "CREATE AGGREGATE a(int) (sfunc = int4pl, stype = int, finalfunc = fs);",
            "function fs(integer) returns a set",
        ),
        (
            "CREATE FUNCTION mysum(int) RETURNS int LANGUAGE sql AS 'select 1';",
            "CREATE OR REPLACE AGGREGATE mysum(int) (sfunc = int4pl, stype = int);",
            "cannot change routine kind",
        ),
    ]);
}

#[test]
fn a_failed_migration_leaves_no_trace() {
    // The migration runs in one transaction: the CREATE FUNCTION before the
    // failing CREATE OR REPLACE is undone too.
    let mut db = PgCatalog::new().unwrap();
    let err = db
        .apply_sql(
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE OR REPLACE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot change name of input parameter \"a\""),
        "{err}"
    );
    assert_err_prefix!(
        db.analyze("SELECT f(a => 1) AS x"),
        AnalyzeError::UndefinedFunction(_),
        "function f(a => integer) does not exist"
    );
}

#[test]
fn inline_bodies_hold_queries_and_transforms_can_be_dropped() {
    assert_ddl_rejections(&[
        (
            "",
            "CREATE FUNCTION f() RETURNS void LANGUAGE sql BEGIN ATOMIC CREATE TABLE x(a int); END;",
            "CREATE TABLE is not yet supported in unquoted SQL function body",
        ),
        (
            "CREATE TRANSFORM FOR int LANGUAGE sql (FROM SQL WITH FUNCTION prsd_lextype(internal));
             DROP TRANSFORM FOR int LANGUAGE sql;",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql TRANSFORM FOR TYPE int AS 'select 1';",
            "transform for type integer language \"sql\" does not exist",
        ),
        (
            "",
            "DROP TRANSFORM FOR int LANGUAGE sql;",
            "transform for type integer language \"sql\" does not exist",
        ),
    ]);
    build(&[(
        "0001.sql",
        "CREATE TRANSFORM FOR int LANGUAGE sql (FROM SQL WITH FUNCTION prsd_lextype(internal));
         CREATE FUNCTION f() RETURNS int LANGUAGE sql TRANSFORM FOR TYPE int AS 'select 1';
         DROP TRANSFORM IF EXISTS FOR text LANGUAGE sql;",
    )]);
}
