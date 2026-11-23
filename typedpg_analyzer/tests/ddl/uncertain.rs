//! `DO` blocks whose effect depends on what the analyzer can't know: what
//! they may change becomes uncertain, and nothing reported afterwards may
//! depend on it (`ddl::uncertain`). Under `pg_sanity` the oracles compare
//! everything that stays known, and accept PostgreSQL's state wherever the
//! catalog is uncertain.

use crate::common::*;

/// The relation `name` in `public`.
fn relation(db: &PgCatalog, name: &str) -> typedpg_analyzer::PgClassOid {
    db.resolve_table(Some("public"), name).unwrap().oid
}

#[test]
fn a_grant_in_a_loop_over_rows_makes_the_grantees_privileges_uncertain() {
    // The loop reads a table: which relations it grants on isn't known, but
    // the grantee is. A query the grant may allow is not denied; another
    // role's privileges stay certain.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE unc_loop_app;
         CREATE ROLE unc_loop_other;
         CREATE TABLE unc_loop_t (id int);
         CREATE TABLE unc_loop_names (name text);
         DO $$
         DECLARE r record;
         BEGIN
           FOR r IN SELECT name FROM unc_loop_names LOOP
             EXECUTE format('GRANT SELECT ON %I TO unc_loop_app', r.name);
           END LOOP;
         END $$;",
    )]);
    db.analyze_as("SELECT id FROM unc_loop_t", "unc_loop_app")
        .unwrap();
    let err = db
        .analyze_as("SELECT id FROM unc_loop_t", "unc_loop_other")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("permission denied for table unc_loop_t"),
        "{err}"
    );
}

#[test]
fn a_revoke_in_a_loop_over_rows_keeps_the_privileges_it_may_take() {
    // A REVOKE that may run can only take privileges away: nothing is
    // denied because of it (PG, whose loop reads no row, keeps the grant).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE unc_revoke_app;
         CREATE TABLE unc_revoke_t (id int);
         CREATE TABLE unc_revoke_names (name text);
         GRANT SELECT ON unc_revoke_t TO unc_revoke_app;
         DO $$
         DECLARE r record;
         BEGIN
           FOR r IN SELECT name FROM unc_revoke_names LOOP
             EXECUTE format('REVOKE SELECT ON %I FROM unc_revoke_app', r.name);
           END LOOP;
         END $$;",
    )]);
    db.analyze_as("SELECT id FROM unc_revoke_t", "unc_revoke_app")
        .unwrap();
}

#[test]
fn a_membership_granted_to_any_role_may_give_its_privileges_to_every_role() {
    // `GRANT grp TO %I` over a table's rows: any role may now be a member of
    // the group, so what the group may do, any role may.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE unc_member_grp;
         CREATE ROLE unc_member_app;
         CREATE TABLE unc_member_t (id int);
         CREATE TABLE unc_member_u (id int);
         CREATE TABLE unc_member_machines (db_role text);
         GRANT SELECT ON unc_member_t TO unc_member_grp;
         DO $$
         DECLARE m record;
         BEGIN
           FOR m IN SELECT db_role FROM unc_member_machines LOOP
             EXECUTE format('GRANT unc_member_grp TO %I', m.db_role);
           END LOOP;
         END $$;",
    )]);
    db.analyze_as("SELECT id FROM unc_member_t", "unc_member_app")
        .unwrap();
    // What the group can't do stays denied.
    let err = db
        .analyze_as("SELECT id FROM unc_member_u", "unc_member_app")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("permission denied for table unc_member_u"),
        "{err}"
    );
}

#[test]
fn privileges_moved_from_the_catalog_reach_the_new_group() {
    // The shape of a migration moving a group's privileges to another one,
    // read from the catalog (`aclexplode`): the outer loop over constants
    // runs, its `CONTINUE WHEN` is decided from the roles the catalog
    // holds, and the loops over the catalog leave the new group's
    // privileges uncertain (granted) and the old one's (revoked). The
    // schema's grant and revoke are certain.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE unc_move_old;
             CREATE ROLE unc_move_new;
             CREATE ROLE unc_move_bystander;
             CREATE TABLE unc_move_t (id int, secret text);
             CREATE TABLE unc_move_u (id int);
             GRANT SELECT ON unc_move_t TO unc_move_old;
             GRANT UPDATE (id) ON unc_move_t TO unc_move_old;",
        ),
        (
            "0002.sql",
            "DO $$
             DECLARE
               pair   record;
               grant_ record;
             BEGIN
               FOR pair IN SELECT * FROM (VALUES ('unc_move_old', 'unc_move_new'),
                                                 ('unc_move_gone', 'unc_move_never'))
                             AS p (old, new) LOOP
                 CONTINUE WHEN NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = pair.old);
                 FOR grant_ IN
                   SELECT c.oid::regclass::text AS object, a.privilege_type AS privilege
                     FROM pg_class AS c, aclexplode(c.relacl) AS a
                    WHERE c.relnamespace = 'public'::regnamespace
                      AND a.grantee = (SELECT oid FROM pg_roles WHERE rolname = pair.old)
                 LOOP
                   EXECUTE format('GRANT %s ON %s TO %I', grant_.privilege, grant_.object,
                                  pair.new);
                   EXECUTE format('REVOKE %s ON %s FROM %I', grant_.privilege, grant_.object,
                                  pair.old);
                 END LOOP;
                 FOR grant_ IN
                   SELECT c.oid::regclass::text AS object, quote_ident(t.attname) AS col,
                          a.privilege_type AS privilege
                     FROM pg_class AS c
                     JOIN pg_attribute AS t
                       ON t.attrelid = c.oid AND t.attnum > 0 AND NOT t.attisdropped,
                          aclexplode(t.attacl) AS a
                    WHERE c.relnamespace = 'public'::regnamespace
                      AND a.grantee = (SELECT oid FROM pg_roles WHERE rolname = pair.old)
                 LOOP
                   EXECUTE format('GRANT %s (%s) ON %s TO %I', grant_.privilege, grant_.col,
                                  grant_.object, pair.new);
                   EXECUTE format('REVOKE %s (%s) ON %s FROM %I', grant_.privilege,
                                  grant_.col, grant_.object, pair.old);
                 END LOOP;
                 EXECUTE format('GRANT USAGE ON SCHEMA public TO %I', pair.new);
                 EXECUTE format('REVOKE USAGE ON SCHEMA public FROM %I', pair.old);
               END LOOP;
             END $$;",
        ),
    ]);
    // Before, the block left the catalog as it was: the new group was
    // denied what PG lets it do.
    db.analyze_as("SELECT id, secret FROM unc_move_t", "unc_move_new")
        .unwrap();
    db.analyze_as("UPDATE unc_move_t SET id = 1", "unc_move_new")
        .unwrap();
    // A role the block never names keeps its certain privileges.
    let err = db
        .analyze_as("SELECT id FROM unc_move_t", "unc_move_bystander")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("permission denied for table unc_move_t"),
        "{err}"
    );
    // The iteration for a role that doesn't exist was skipped: nothing was
    // granted to `unc_move_never` (which `GRANT` would have refused).
    assert!(!db.role_exists("unc_move_never"));
}

#[test]
fn an_if_on_the_catalog_runs_the_branch_the_catalog_selects_and_the_rest_runs() {
    // `IF EXISTS (SELECT FROM pg_publication ...)` is decided from the
    // catalog: no such publication, so the branch doesn't run — and the
    // statements after the IF do (they used to be dropped with the block).
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE unc_if_t (a int, b int);
         DO $$
         BEGIN
           IF EXISTS (SELECT 1 FROM pg_publication WHERE pubname = 'unc_if_pub'
                      AND NOT puballtables) THEN
             EXECUTE 'ALTER PUBLICATION unc_if_pub DROP TABLE public.unc_if_t';
           END IF;
           EXECUTE 'ALTER TABLE unc_if_t DROP COLUMN b';
           IF EXISTS (SELECT 1 FROM pg_publication_tables WHERE pubname = 'unc_if_pub'
                      AND schemaname = 'public' AND tablename = 'unc_if_t') THEN
             RAISE EXCEPTION 'unreached';
           END IF;
         END $$;",
    )]);
    let err = db.analyze("SELECT b FROM unc_if_t").unwrap_err();
    assert!(
        err.to_string().starts_with("column \"b\" does not exist"),
        "{err}"
    );
}

#[test]
fn statements_after_an_undecidable_if_still_run() {
    // The condition reads a table: the IF is uncertain (its branch may
    // grant), what follows it certainly runs.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE unc_after_app;
         CREATE TABLE unc_after_t (id int);
         CREATE TABLE unc_after_flags (on_ boolean);
         DO $$
         BEGIN
           IF EXISTS (SELECT 1 FROM unc_after_flags WHERE on_) THEN
             GRANT SELECT ON unc_after_t TO unc_after_app;
           END IF;
           CREATE TABLE unc_after_u (id int);
         END $$;",
    )]);
    db.analyze("SELECT id FROM unc_after_u").unwrap();
    db.analyze_as("SELECT id FROM unc_after_t", "unc_after_app")
        .unwrap();
}

#[test]
fn create_role_unless_it_exists_through_an_exception_handler() {
    // The second block's CREATE ROLE fails; whether `duplicate_object`
    // catches it isn't known to the analyzer, but the handler does
    // nothing, so nothing is uncertain.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE TABLE unc_dup_t (id int);
             DO $$ BEGIN CREATE ROLE unc_dup_r NOLOGIN;
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$;",
        ),
        (
            "0002.sql",
            "DO $$ BEGIN CREATE ROLE unc_dup_r NOLOGIN;
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$;
             GRANT SELECT ON unc_dup_t TO unc_dup_r;",
        ),
    ]);
    assert!(db.role_exists("unc_dup_r"));
    db.analyze_as("SELECT id FROM unc_dup_t", "unc_dup_r")
        .unwrap();
    let err = db
        .analyze_as("INSERT INTO unc_dup_t VALUES (1)", "unc_dup_r")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("permission denied for table unc_dup_t"),
        "{err}"
    );
}

#[test]
fn loops_over_constants_run_each_iteration() {
    // FOREACH over an array of constants, FOR over VALUES and over an
    // integer range run iteration by iteration, `format()` building each
    // statement.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE unc_const_app;
         DO $$
         DECLARE t text; r record; i int;
         BEGIN
           FOREACH t IN ARRAY ARRAY['unc_const_a', 'unc_const_b'] LOOP
             EXECUTE format('CREATE TABLE %I (id int)', t);
             IF t = 'unc_const_b' THEN
               EXECUTE format('COMMENT ON TABLE %I IS %L', t, 'the second');
             END IF;
           END LOOP;
           FOR r IN SELECT * FROM (VALUES ('unc_const_a', 'SELECT'),
                                          ('unc_const_b', 'INSERT')) AS v (tab, priv) LOOP
             EXECUTE format('GRANT %s ON %I TO %I', r.priv, r.tab, 'unc_const_app');
           END LOOP;
           FOR i IN 1..2 LOOP
             EXECUTE format('CREATE TABLE %1$I_%2$s (id int)', 'unc_const_n', i);
           END LOOP;
         END $$;",
    )]);
    for table in [
        "unc_const_a",
        "unc_const_b",
        "unc_const_n_1",
        "unc_const_n_2",
    ] {
        relation(&db, table);
    }
    db.analyze_as("SELECT id FROM unc_const_a", "unc_const_app")
        .unwrap();
    db.analyze_as("INSERT INTO unc_const_b VALUES (1)", "unc_const_app")
        .unwrap();
    // Known, so certain: the grants were exactly these.
    let err = db
        .analyze_as("SELECT id FROM unc_const_b", "unc_const_app")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("permission denied for table unc_const_b"),
        "{err}"
    );
}

#[test]
fn search_path_saved_and_restored_through_variables() {
    // An extension script's idiom: save the setting, change it, restore
    // it from the variable.
    let db = build_db(&[(
        "0001.sql",
        "CREATE SCHEMA unc_path;
         SET search_path = unc_path, public;
         DO $$
         DECLARE old_path text := pg_catalog.current_setting('search_path');
         BEGIN
           PERFORM pg_catalog.set_config('search_path', 'public', true);
           PERFORM pg_catalog.set_config('search_path', old_path, true);
         END $$;
         CREATE TABLE unc_path_t (id int);",
    )]);
    db.resolve_table(Some("unc_path"), "unc_path_t").unwrap();
}

#[test]
fn a_fully_unknown_execute_makes_later_failures_uncertain() {
    // The string comes from a table: it may have created anything, so a
    // later statement failing on what it may have created is no error.
    try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE unc_any_stmts (sql text);
             DO $$
             DECLARE r record;
             BEGIN
               FOR r IN SELECT sql FROM unc_any_stmts LOOP
                 EXECUTE r.sql;
               END LOOP;
             END $$;",
        ),
        ("0002.sql", "ALTER TABLE unc_any_maybe ADD COLUMN x int;"),
    ])
    .unwrap();
}

#[test]
fn routines_redefined_from_their_definition_stay_defined() {
    // `pg_get_functiondef`, rewritten and executed: the routine's
    // definition is uncertain, its existence is not.
    let db = build_db(&[(
        "0001.sql",
        "CREATE FUNCTION unc_def_f() RETURNS text LANGUAGE sql AS $$ SELECT 'old' $$;
         DO $$
         DECLARE f record; rewritten text;
         BEGIN
           FOR f IN SELECT pg_get_functiondef(p.oid) AS definition
                      FROM pg_proc AS p WHERE p.proname = 'unc_def_f' LOOP
             rewritten := replace(f.definition, '''old''', '''new''');
             IF rewritten = f.definition THEN
               RAISE EXCEPTION 'nothing to rewrite';
             END IF;
             EXECUTE rewritten;
           END LOOP;
         END $$;
         CREATE TABLE unc_def_t (id int);",
    )]);
    db.analyze("SELECT unc_def_f()").unwrap();
    db.analyze("SELECT id FROM unc_def_t").unwrap();
}

#[test]
fn triggers_toggled_over_unknown_tables_stay_defined() {
    // `ALTER TABLE %I DISABLE TRIGGER USER` over tables a query picked:
    // which triggers fire is uncertain, nothing else.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE unc_trig_t (id int);
         CREATE FUNCTION unc_trig_f() RETURNS trigger LANGUAGE plpgsql
           AS $$ BEGIN RETURN NEW; END $$;
         CREATE TRIGGER unc_trig BEFORE INSERT ON unc_trig_t
           FOR EACH ROW EXECUTE FUNCTION unc_trig_f();
         DO $$
         DECLARE one text;
         BEGIN
           FOR one IN SELECT relname FROM pg_class WHERE relname LIKE 'unc_trig%' AND relkind = 'r'
           LOOP
             EXECUTE format('ALTER TABLE %I DISABLE TRIGGER USER', one);
           END LOOP;
         END $$;
         CREATE TABLE unc_trig_after (id int);",
    )]);
    db.analyze("SELECT id FROM unc_trig_after").unwrap();
}
