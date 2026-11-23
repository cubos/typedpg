//! CREATE / DROP PROCEDURE: procedures are stored distinct from functions,
//! cannot be called inside expressions, accept OUT parameters.

use crate::common::*;

// ── CREATE PROCEDURE ────────────────────────────────────────────────────────

#[test]
fn create_procedure_registers_with_is_procedure_flag() {
    let snap = build(&[(
        "0001.sql",
        "CREATE PROCEDURE do_thing(x int) LANGUAGE SQL AS $$ SELECT $1 $$;",
    )]);

    let public_oid = snap.namespace_oid("public").unwrap();
    let procs: Vec<&PgProc> = snap
        .pg_proc()
        .values()
        .filter(|p| p.pronamespace == public_oid && p.proname == "do_thing")
        .collect();
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0].prokind, ProKind::Procedure);
}

#[test]
fn procedure_is_not_callable_in_expressions() {
    // PG sanity returns a protocol-level "unexpected message" without a real
    // SQLSTATE for this case, so the sanity mirror's prefix check is
    // automatically skipped (no `DbError`). The analyzer asserts the
    // SQLSTATE 42809 wording PG would use here.
    let db = build_db(&[(
        "0001.sql",
        "CREATE PROCEDURE do_thing(x int) LANGUAGE SQL AS $$ SELECT $1 $$;",
    )]);

    assert_analyze_err!(
        db.analyze("SELECT do_thing(1)"),
        AnalyzeError::WrongObjectType(_),
        "\
do_thing(integer) is a procedure
  ╭────
1 │ SELECT do_thing(1)
  ·        ────────
  ╰────
  help: to call a procedure, use CALL
",
    );
}

// ── DROP PROCEDURE vs function of same name ────────────────────────────────

#[test]
fn drop_procedure_does_not_touch_function_of_same_name() {
    // Function and procedure share `pg_proc`'s name+args namespace, so they
    // must take different signatures (PG SQLSTATE 42723 otherwise).
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION f(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE PROCEDURE f(x text) LANGUAGE SQL AS $$ SELECT 1 $$;
         DROP PROCEDURE f(text);",
    )]);

    let public_oid = snap.namespace_oid("public").unwrap();
    let procs: Vec<&PgProc> = snap
        .pg_proc()
        .values()
        .filter(|p| p.pronamespace == public_oid && p.proname == "f")
        .collect();
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0].prokind, ProKind::Function);
}

// ── A migration's SELECT of a PL/pgSQL function runs its body ──────────────

const MAKE_QUEUE: &str = "CREATE FUNCTION queue_name(name text, prefix text) RETURNS text
         LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
             RETURN lower(prefix || '_' || name);
         END $$;
     CREATE FUNCTION make_queue(name text) RETURNS void
         LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
         DECLARE
             qtable text := public.queue_name(name, 'Q');
         BEGIN
             IF length(name) > 10 THEN
                 RAISE EXCEPTION 'queue name too long';
             END IF;
             EXECUTE format('CREATE TABLE public.%I (id int NOT NULL)', qtable);
             IF to_regclass(format('public.%I', qtable || '_idx')) IS NULL THEN
                 EXECUTE format('CREATE INDEX %I ON public.%I (id)', qtable || '_idx', qtable);
             END IF;
             PERFORM public.make_archive(name);
         END $$;
     CREATE FUNCTION make_archive(name text) RETURNS void
         LANGUAGE plpgsql AS $$
         BEGIN
             EXECUTE format('CREATE TABLE public.%I (id int)', 'a_' || name);
         END $$;";

#[test]
fn select_of_a_plpgsql_function_runs_its_body() {
    // ExecuteQuery runs the function: its EXECUTEs of strings built from
    // its arguments, a nested function's RETURN value and a PERFORM of
    // another function (pgmq.create's shape) create the tables.
    let db = build_db(&[
        ("0001.sql", MAKE_QUEUE),
        ("0002.sql", "SELECT make_queue('orders');"),
    ]);
    assert!(db.analyze("SELECT id FROM q_orders").is_ok());
    assert!(db.analyze("SELECT id FROM a_orders").is_ok());
    assert!(db.resolve_table(Some("public"), "q_orders_idx").is_some());
    // The function's RAISE runs for a long name.
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", MAKE_QUEUE),
            ("0002.sql", "SELECT make_queue('a_very_long_queue');"),
        ]),
        DdlError::UnsupportedDdl(_),
        "queue name too long",
    );
}

#[test]
fn do_block_reads_custom_settings_and_extensions() {
    // current_setting of a custom setting the session set (or never did,
    // with missing_ok), EXISTS over pg_extension and a scalar subquery of
    // a condition are decided: the block runs certainly.
    let db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION citext;
         SELECT set_config('app.flag', 'on', false);
         DO $$
         BEGIN
             IF current_setting('app.flag', true) = 'on' THEN
                 CREATE TABLE flagged (x int);
             END IF;
             IF current_setting('app.missing', true) IS NULL THEN
                 CREATE TABLE unset (x int);
             END IF;
             IF (SELECT NOT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'citext')) THEN
                 CREATE TABLE no_citext (x int);
             END IF;
             IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'citext') THEN
                 CREATE TABLE has_citext (x int);
             END IF;
         END $$;",
    )]);
    for (table, exists) in [
        ("flagged", true),
        ("unset", true),
        ("no_citext", false),
        ("has_citext", true),
    ] {
        assert_eq!(
            db.resolve_table(Some("public"), table).is_some(),
            exists,
            "{table}"
        );
    }
}
