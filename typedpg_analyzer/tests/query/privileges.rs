//! `PgCatalog::analyze_as`: whether a role the migrations create may run a
//! statement, as PostgreSQL decides when that role plans and executes it.
//! Under `pg_sanity` every check is mirrored with `SET LOCAL ROLE`.
//!
//! Roles live in the cluster the mirror's tests share: each test's roles
//! carry a prefix of its own.

use crate::common::*;

/// The error `analyze_as` reports, as text.
#[track_caller]
fn denied(db: &PgCatalog, role: &str, sql: &str) -> String {
    match db.analyze_as(sql, role) {
        Ok(_) => panic!("expected {role} to be denied: {sql}"),
        Err(e) => e.to_string(),
    }
}

#[track_caller]
fn allowed(db: &PgCatalog, role: &str, sql: &str) {
    if let Err(e) = db.analyze_as(sql, role) {
        panic!("expected {role} to be allowed: {sql}\n{e}");
    }
}

/// Assert the error starts with PG's message.
#[track_caller]
fn denied_with(db: &PgCatalog, role: &str, sql: &str, prefix: &str) -> String {
    let e = denied(db, role, sql);
    assert!(
        e.starts_with(prefix),
        "{role} running: {sql}\n  expected prefix: {prefix}\n             got: {e}"
    );
    e
}

#[test]
fn smoke_table_and_column_privileges() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_smoke_app;
         CREATE TABLE t (id serial PRIMARY KEY, a int, b int, c int);
         GRANT SELECT (a) ON t TO qp_smoke_app;
         GRANT INSERT (a, b) ON t TO qp_smoke_app;
         GRANT UPDATE (b) ON t TO qp_smoke_app;",
    )]);
    allowed(&db, "qp_smoke_app", "SELECT a FROM t");
    allowed(&db, "qp_smoke_app", "SELECT count(*) FROM t");
    let e = denied(&db, "qp_smoke_app", "SELECT b FROM t");
    assert!(e.starts_with("permission denied for table t"), "{e}");
    denied(&db, "qp_smoke_app", "SELECT * FROM t");
    denied(&db, "qp_smoke_app", "SELECT t FROM t");
    allowed(&db, "qp_smoke_app", "UPDATE t SET b = 1 WHERE a = 1");
    denied(&db, "qp_smoke_app", "UPDATE t SET b = 1 WHERE c = 1");
    denied(&db, "qp_smoke_app", "DELETE FROM t");
    let e = denied(&db, "qp_smoke_app", "INSERT INTO t (a, b) VALUES (1, 2)");
    assert!(
        e.starts_with("permission denied for sequence t_id_seq"),
        "{e}"
    );
}

#[test]
fn select_table_privilege() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_sel_app;
         CREATE ROLE qp_sel_none;
         CREATE TABLE qp_sel_t (id int, a int);
         GRANT SELECT ON qp_sel_t TO qp_sel_app;",
    )]);
    for sql in [
        "SELECT id, a FROM qp_sel_t",
        "SELECT count(*) FROM qp_sel_t",
        "SELECT id FROM qp_sel_t WHERE a > 0",
        "SELECT id FROM qp_sel_t ORDER BY a",
        "SELECT x.id FROM qp_sel_t x JOIN qp_sel_t y ON x.id = y.a",
        "SELECT id FROM qp_sel_t WHERE EXISTS (SELECT 1 FROM qp_sel_t u WHERE u.a = qp_sel_t.id)",
        "SELECT DISTINCT a FROM qp_sel_t",
        "SELECT a FROM qp_sel_t GROUP BY a",
        "SELECT a, count(*) FROM qp_sel_t GROUP BY a HAVING count(*) > 1",
    ] {
        allowed(&db, "qp_sel_app", sql);
    }
    denied_with(
        &db,
        "qp_sel_none",
        "SELECT id FROM qp_sel_t",
        "permission denied for table qp_sel_t",
    );
}

#[test]
fn select_column_privileges() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_col_app;
         CREATE TABLE qp_col_t (id int, a int, b int);
         GRANT SELECT (a) ON qp_col_t TO qp_col_app;",
    )]);
    for sql in [
        "SELECT a FROM qp_col_t",
        "SELECT count(*) FROM qp_col_t",
        "SELECT a FROM qp_col_t WHERE a > 0",
        "SELECT a FROM qp_col_t ORDER BY a",
        "SELECT a FROM qp_col_t GROUP BY a",
    ] {
        allowed(&db, "qp_col_app", sql);
    }
    for (sql, prefix) in [
        (
            "SELECT b FROM qp_col_t",
            "permission denied for table qp_col_t",
        ),
        (
            "SELECT a FROM qp_col_t WHERE b > 0",
            "permission denied for table qp_col_t",
        ),
        (
            "SELECT a FROM qp_col_t ORDER BY b",
            "permission denied for table qp_col_t",
        ),
        (
            "SELECT x.a FROM qp_col_t x JOIN qp_col_t y ON x.a = y.b",
            "permission denied for table qp_col_t",
        ),
        (
            "SELECT a FROM qp_col_t WHERE EXISTS (SELECT 1 FROM qp_col_t u WHERE u.b > 0)",
            "permission denied for table qp_col_t",
        ),
        (
            "SELECT * FROM qp_col_t",
            "permission denied for table qp_col_t",
        ),
        (
            "SELECT qp_col_t.* FROM qp_col_t",
            "permission denied for table qp_col_t",
        ),
        (
            "SELECT qp_col_t FROM qp_col_t",
            "permission denied for table qp_col_t",
        ),
        // No column privilege covers a system column: it needs SELECT on
        // the relation itself.
        (
            "SELECT ctid FROM qp_col_t",
            "permission denied for table qp_col_t",
        ),
    ] {
        denied_with(&db, "qp_col_app", sql, prefix);
    }
}

#[test]
fn insert_target_columns_and_defaults() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_ins_app;
         CREATE ROLE qp_ins_none;
         CREATE ROLE qp_ins_defcol;
         CREATE TABLE qp_ins_t (a int, b int DEFAULT 5, c int DEFAULT 6);
         CREATE TABLE qp_ins_src (x int, y int);
         INSERT INTO qp_ins_src VALUES (1, 2);
         GRANT INSERT (a) ON qp_ins_t TO qp_ins_app;
         GRANT INSERT (b, c) ON qp_ins_t TO qp_ins_defcol;
         GRANT SELECT (x) ON qp_ins_src TO qp_ins_app;",
    )]);
    for sql in [
        // An omitted column needs no INSERT privilege, only its default.
        "INSERT INTO qp_ins_t (a) VALUES (1)",
        // No target columns at all: any column's INSERT privilege suffices.
        "INSERT INTO qp_ins_t DEFAULT VALUES",
        "INSERT INTO qp_ins_t (a) SELECT x FROM qp_ins_src",
    ] {
        allowed(&db, "qp_ins_app", sql);
    }
    for (sql, prefix) in [
        (
            "INSERT INTO qp_ins_t (a, b) VALUES (1, 2)",
            "permission denied for table qp_ins_t",
        ),
        (
            "INSERT INTO qp_ins_t VALUES (1, 2, 3)",
            "permission denied for table qp_ins_t",
        ),
        // `DEFAULT` in a VALUES row without a column list still names the
        // column: INSERT is required on it (`a`, here granted, but `b` is
        // not).
        (
            "INSERT INTO qp_ins_t VALUES (DEFAULT, 2, 3)",
            "permission denied for table qp_ins_t",
        ),
        (
            "INSERT INTO qp_ins_t (a) SELECT y FROM qp_ins_src",
            "permission denied for table qp_ins_src",
        ),
        (
            "INSERT INTO qp_ins_t (a) SELECT x FROM qp_ins_src RETURNING b",
            "permission denied for table qp_ins_t",
        ),
    ] {
        denied_with(&db, "qp_ins_app", sql, prefix);
    }
    denied_with(
        &db,
        "qp_ins_none",
        "INSERT INTO qp_ins_t (a) VALUES (1)",
        "permission denied for table qp_ins_t",
    );
    // A VALUES row without a column list names every column, `DEFAULT`
    // positions included: omitting `a` from the column list is fine, but
    // writing `DEFAULT` in its place still needs INSERT on it.
    allowed(
        &db,
        "qp_ins_defcol",
        "INSERT INTO qp_ins_t (b, c) VALUES (2, 3)",
    );
    denied_with(
        &db,
        "qp_ins_defcol",
        "INSERT INTO qp_ins_t VALUES (DEFAULT, 2, 3)",
        "permission denied for table qp_ins_t",
    );
}

#[test]
fn insert_column_defaults_reach_sequences_and_identity() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_def_app;
         CREATE ROLE qp_def_seq_app;
         CREATE ROLE qp_def_explicit;
         CREATE ROLE qp_def_ident_app;
         CREATE TABLE qp_def_t (id serial PRIMARY KEY, a int);
         CREATE TABLE qp_def_ident (id int GENERATED ALWAYS AS IDENTITY, a int);
         GRANT INSERT (a) ON qp_def_t TO qp_def_app;
         GRANT INSERT (a) ON qp_def_t TO qp_def_seq_app;
         GRANT USAGE ON SEQUENCE qp_def_t_id_seq TO qp_def_seq_app;
         GRANT INSERT (id, a) ON qp_def_t TO qp_def_explicit;
         GRANT INSERT (a) ON qp_def_ident TO qp_def_ident_app;",
    )]);
    // The serial default's nextval needs the sequence's USAGE (or UPDATE).
    denied_with(
        &db,
        "qp_def_app",
        "INSERT INTO qp_def_t (a) VALUES (1)",
        "permission denied for sequence qp_def_t_id_seq",
    );
    denied_with(
        &db,
        "qp_def_app",
        "INSERT INTO qp_def_t DEFAULT VALUES",
        "permission denied for sequence qp_def_t_id_seq",
    );
    allowed(&db, "qp_def_seq_app", "INSERT INTO qp_def_t (a) VALUES (1)");
    // Giving the column explicitly skips its default.
    allowed(
        &db,
        "qp_def_explicit",
        "INSERT INTO qp_def_t (id, a) VALUES (1, 1)",
    );
    // An identity column's default belongs to the table: no sequence
    // privilege is involved.
    allowed(
        &db,
        "qp_def_ident_app",
        "INSERT INTO qp_def_ident (a) VALUES (1)",
    );
}

#[test]
fn sequence_functions() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_seq_none;
         CREATE ROLE qp_seq_usage;
         CREATE ROLE qp_seq_update;
         CREATE ROLE qp_seq_select;
         CREATE SEQUENCE qp_seq_s;
         GRANT USAGE ON SEQUENCE qp_seq_s TO qp_seq_usage;
         GRANT UPDATE ON SEQUENCE qp_seq_s TO qp_seq_update;
         GRANT SELECT ON SEQUENCE qp_seq_s TO qp_seq_select;",
    )]);
    for sql in [
        "SELECT nextval('qp_seq_s')",
        "SELECT currval('qp_seq_s')",
        "SELECT setval('qp_seq_s', 1)",
    ] {
        denied_with(
            &db,
            "qp_seq_none",
            sql,
            "permission denied for sequence qp_seq_s",
        );
    }
    // nextval accepts USAGE or UPDATE; currval SELECT or USAGE.
    allowed(&db, "qp_seq_usage", "SELECT nextval('qp_seq_s')");
    allowed(
        &db,
        "qp_seq_usage",
        "SELECT nextval('qp_seq_s'), currval('qp_seq_s')",
    );
    denied_with(
        &db,
        "qp_seq_usage",
        "SELECT setval('qp_seq_s', 1)",
        "permission denied for sequence qp_seq_s",
    );
    allowed(&db, "qp_seq_update", "SELECT nextval('qp_seq_s')");
    allowed(&db, "qp_seq_update", "SELECT setval('qp_seq_s', 1)");
    denied_with(
        &db,
        "qp_seq_update",
        "SELECT currval('qp_seq_s')",
        "permission denied for sequence qp_seq_s",
    );
    allowed(&db, "qp_seq_select", "SELECT currval('qp_seq_s')");
    denied_with(
        &db,
        "qp_seq_select",
        "SELECT nextval('qp_seq_s')",
        "permission denied for sequence qp_seq_s",
    );
}

#[test]
fn update_set_and_read_columns() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_upd_app;
         CREATE ROLE qp_upd_none;
         CREATE TABLE qp_upd_t (id int, a int, b int);
         GRANT UPDATE (a), SELECT (id) ON qp_upd_t TO qp_upd_app;",
    )]);
    for sql in [
        "UPDATE qp_upd_t SET a = 1",
        "UPDATE qp_upd_t SET a = 1 WHERE id = 1",
        "UPDATE qp_upd_t SET a = 1 RETURNING id",
    ] {
        allowed(&db, "qp_upd_app", sql);
    }
    for sql in [
        // Reading a column in WHERE / a SET expression / RETURNING needs
        // SELECT on it.
        "UPDATE qp_upd_t SET a = 1 WHERE b = 1",
        "UPDATE qp_upd_t SET a = b",
        "UPDATE qp_upd_t SET a = 1 RETURNING b",
        // RETURNING needs SELECT even of a column the UPDATE writes.
        "UPDATE qp_upd_t SET a = 1 RETURNING a",
        // Writing an ungranted column needs UPDATE on it.
        "UPDATE qp_upd_t SET b = 1",
    ] {
        denied_with(
            &db,
            "qp_upd_app",
            sql,
            "permission denied for table qp_upd_t",
        );
    }
    denied_with(
        &db,
        "qp_upd_none",
        "UPDATE qp_upd_t SET a = 1",
        "permission denied for table qp_upd_t",
    );
}

#[test]
fn update_set_default_runs_the_default() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_udef_app;
         CREATE ROLE qp_udef_seq;
         CREATE TABLE qp_udef_t (id serial, a int);
         INSERT INTO qp_udef_t (a) VALUES (1);
         GRANT UPDATE (id) ON qp_udef_t TO qp_udef_app;
         GRANT UPDATE (id) ON qp_udef_t TO qp_udef_seq;
         GRANT USAGE ON SEQUENCE qp_udef_t_id_seq TO qp_udef_seq;",
    )]);
    denied_with(
        &db,
        "qp_udef_app",
        "UPDATE qp_udef_t SET id = DEFAULT",
        "permission denied for sequence qp_udef_t_id_seq",
    );
    allowed(&db, "qp_udef_seq", "UPDATE qp_udef_t SET id = DEFAULT");
}

#[test]
fn delete_reads_where_and_returning_columns() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_del_app;
         CREATE ROLE qp_del_none;
         CREATE TABLE qp_del_t (id int, a int);
         GRANT DELETE ON qp_del_t TO qp_del_app;
         GRANT SELECT (id) ON qp_del_t TO qp_del_app;",
    )]);
    for sql in [
        "DELETE FROM qp_del_t",
        "DELETE FROM qp_del_t WHERE id = 1",
        "DELETE FROM qp_del_t RETURNING id",
    ] {
        allowed(&db, "qp_del_app", sql);
    }
    for sql in [
        "DELETE FROM qp_del_t WHERE a = 1",
        "DELETE FROM qp_del_t RETURNING a",
    ] {
        denied_with(
            &db,
            "qp_del_app",
            sql,
            "permission denied for table qp_del_t",
        );
    }
    denied_with(
        &db,
        "qp_del_none",
        "DELETE FROM qp_del_t",
        "permission denied for table qp_del_t",
    );
}

#[test]
fn insert_on_conflict_arbiters_and_do_update() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_conf_app;
         CREATE ROLE qp_conf_noselect;
         CREATE ROLE qp_conf_partial;
         CREATE TABLE qp_conf_t (id int PRIMARY KEY, a int, b int);
         GRANT INSERT (id, a, b), UPDATE (a), SELECT (id, a, b) ON qp_conf_t TO qp_conf_app;
         GRANT INSERT (id, a, b), UPDATE (a) ON qp_conf_t TO qp_conf_noselect;
         GRANT INSERT (id, a, b), UPDATE (a), SELECT (id) ON qp_conf_t TO qp_conf_partial;",
    )]);
    for sql in [
        "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT DO NOTHING",
        "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT (id) DO NOTHING",
        "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT ON CONSTRAINT qp_conf_t_pkey DO NOTHING",
        "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a",
        "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a WHERE qp_conf_t.id = 1",
        "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = qp_conf_t.b",
    ] {
        allowed(&db, "qp_conf_app", sql);
    }
    for (sql, prefix) in [
        // The arbiter's columns are read: SELECT on them.
        (
            "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT (id) DO NOTHING",
            "permission denied for table qp_conf_t",
        ),
        (
            "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT ON CONSTRAINT qp_conf_t_pkey DO NOTHING",
            "permission denied for table qp_conf_t",
        ),
        // ... and needs UPDATE on the column it writes.
        (
            "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET b = EXCLUDED.b",
            "permission denied for table qp_conf_t",
        ),
    ] {
        denied_with(&db, "qp_conf_noselect", sql, prefix);
    }
    // DO UPDATE reads the target column named in its SET expression, which
    // needs SELECT. `EXCLUDED` has the target relation's
    // RTEPermissionInfo (only the relation's kind is changed), so
    // `excluded.a` reads `a` just the same; this role has the arbiter's
    // `id`, not `a`.
    for sql in [
        "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = qp_conf_t.b",
        "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a",
    ] {
        denied_with(
            &db,
            "qp_conf_partial",
            sql,
            "permission denied for table qp_conf_t",
        );
    }
    // What `EXCLUDED` needs is SELECT on the columns read from it; the
    // pseudo-relation itself adds no privilege.
    allowed(
        &db,
        "qp_conf_app",
        "INSERT INTO qp_conf_t (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a",
    );
}

#[test]
fn insert_on_conflict_do_select_locking_needs_update() {
    // PostgreSQL 19: DO SELECT reads the conflicting row (SELECT on what
    // RETURNING and the arbiter read); FOR UPDATE / SHARE locks it, which
    // requires UPDATE on the table (any column of it).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dosel_app;
         CREATE ROLE qp_dosel_lock;
         CREATE TABLE qp_dosel_t (id int PRIMARY KEY, a int);
         GRANT INSERT, SELECT ON qp_dosel_t TO qp_dosel_app;
         GRANT INSERT, SELECT, UPDATE (a) ON qp_dosel_t TO qp_dosel_lock;",
    )]);
    let select = "INSERT INTO qp_dosel_t VALUES (1, 1) ON CONFLICT (id) DO SELECT RETURNING a";
    allowed(&db, "qp_dosel_app", select);
    for sql in [
        "INSERT INTO qp_dosel_t VALUES (1, 1) ON CONFLICT (id) DO SELECT FOR UPDATE RETURNING a",
        "INSERT INTO qp_dosel_t VALUES (1, 1) ON CONFLICT (id) DO SELECT FOR KEY SHARE RETURNING a",
    ] {
        denied_with(
            &db,
            "qp_dosel_app",
            sql,
            "permission denied for table qp_dosel_t",
        );
        allowed(&db, "qp_dosel_lock", sql);
    }
}

#[test]
fn merge_target_privileges() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_merge_app;
         CREATE ROLE qp_merge_join;
         CREATE ROLE qp_merge_ins;
         CREATE ROLE qp_merge_del;
         CREATE ROLE qp_merge_none;
         CREATE TABLE qp_merge_t (id int PRIMARY KEY, a int, b int);
         CREATE TABLE qp_merge_s (id int, a int, b int);
         GRANT SELECT ON qp_merge_s TO qp_merge_app;
         GRANT SELECT ON qp_merge_s TO qp_merge_join;
         GRANT SELECT ON qp_merge_s TO qp_merge_ins;
         GRANT SELECT ON qp_merge_s TO qp_merge_del;
         GRANT SELECT ON qp_merge_s TO qp_merge_none;
         GRANT UPDATE (a), SELECT (id) ON qp_merge_t TO qp_merge_app;
         GRANT UPDATE (a) ON qp_merge_t TO qp_merge_join;
         GRANT INSERT (id, a), SELECT (id) ON qp_merge_t TO qp_merge_ins;
         GRANT DELETE, SELECT (id) ON qp_merge_t TO qp_merge_del;",
    )]);
    // The join condition reads the target's `id`; the action writes `a`.
    allowed(
        &db,
        "qp_merge_app",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET a = s.a",
    );
    denied_with(
        &db,
        "qp_merge_app",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET b = s.b",
        "permission denied for table qp_merge_t",
    );
    // No SELECT on the join's target columns.
    denied_with(
        &db,
        "qp_merge_join",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET a = s.a",
        "permission denied for table qp_merge_t",
    );
    allowed(
        &db,
        "qp_merge_ins",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON t.id = s.id \
         WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
    );
    denied_with(
        &db,
        "qp_merge_ins",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON t.id = s.id \
         WHEN NOT MATCHED THEN INSERT (id, b) VALUES (s.id, s.b)",
        "permission denied for table qp_merge_t",
    );
    allowed(
        &db,
        "qp_merge_del",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON t.id = s.id \
         WHEN MATCHED THEN DELETE",
    );
    denied_with(
        &db,
        "qp_merge_none",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON t.id = s.id \
         WHEN MATCHED THEN DELETE",
        "permission denied for table qp_merge_t",
    );
    // DO NOTHING still requires SELECT on the target, even when no other
    // clause reads it.
    allowed(
        &db,
        "qp_merge_del",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON t.id = s.id \
         WHEN MATCHED THEN DO NOTHING",
    );
    denied_with(
        &db,
        "qp_merge_join",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON t.id = s.id \
         WHEN MATCHED THEN DO NOTHING",
        "permission denied for table qp_merge_t",
    );
    denied_with(
        &db,
        "qp_merge_none",
        "MERGE INTO qp_merge_t t USING qp_merge_s s ON false \
         WHEN MATCHED THEN DO NOTHING",
        "permission denied for table qp_merge_t",
    );
}

#[test]
fn row_locking_needs_update_privilege() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_lock_app;
         CREATE ROLE qp_lock_sel;
         CREATE ROLE qp_lock_none;
         CREATE TABLE qp_lock_t (id int, a int);
         CREATE TABLE qp_lock_u (id int, b int);
         GRANT SELECT (id), UPDATE (a) ON qp_lock_t TO qp_lock_app;
         GRANT SELECT (id) ON qp_lock_u TO qp_lock_app;
         GRANT SELECT (id) ON qp_lock_t TO qp_lock_sel;
         GRANT SELECT (id) ON qp_lock_u TO qp_lock_sel;",
    )]);
    for sql in [
        "SELECT id FROM qp_lock_t FOR UPDATE",
        "SELECT id FROM qp_lock_t FOR SHARE",
        "SELECT t.id FROM qp_lock_t t, qp_lock_u u WHERE t.id = u.id FOR UPDATE OF t",
        "SELECT t.id FROM qp_lock_t t, qp_lock_u u WHERE t.id = u.id FOR SHARE OF t",
    ] {
        allowed(&db, "qp_lock_app", sql);
    }
    // Any column's UPDATE privilege satisfies the row lock, but SELECT on
    // the relation is still required.
    denied_with(
        &db,
        "qp_lock_sel",
        "SELECT id FROM qp_lock_t FOR UPDATE",
        "permission denied for table qp_lock_t",
    );
    denied_with(
        &db,
        "qp_lock_app",
        "SELECT t.id FROM qp_lock_t t, qp_lock_u u WHERE t.id = u.id FOR UPDATE OF u",
        "permission denied for table qp_lock_u",
    );
    denied_with(
        &db,
        "qp_lock_none",
        "SELECT id FROM qp_lock_t FOR UPDATE",
        "permission denied for table qp_lock_t",
    );
}

#[test]
fn ctes_and_data_modifying_ctes() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_cte_app;
         CREATE TABLE qp_cte_t (id int, a int);
         CREATE TABLE qp_cte_hidden (id int, b int);
         GRANT SELECT (id), INSERT (id), UPDATE (id), DELETE ON qp_cte_t TO qp_cte_app;",
    )]);
    for sql in [
        "WITH x AS (SELECT id FROM qp_cte_t) SELECT id FROM x",
        "WITH x AS (SELECT id FROM qp_cte_t), y AS (SELECT id FROM x) SELECT id FROM y",
        "WITH d AS (DELETE FROM qp_cte_t RETURNING id) SELECT id FROM d",
        "WITH i AS (INSERT INTO qp_cte_t (id) VALUES (1) RETURNING id) SELECT id FROM i",
        "WITH u AS (UPDATE qp_cte_t SET id = 1 RETURNING id) SELECT id FROM u",
    ] {
        allowed(&db, "qp_cte_app", sql);
    }
    for (sql, prefix) in [
        (
            "WITH x AS (SELECT b FROM qp_cte_hidden) SELECT b FROM x",
            "permission denied for table qp_cte_hidden",
        ),
        (
            "WITH x AS (SELECT id FROM qp_cte_t WHERE id IN (SELECT b FROM qp_cte_hidden)) SELECT id FROM x",
            "permission denied for table qp_cte_hidden",
        ),
        (
            "WITH d AS (DELETE FROM qp_cte_t RETURNING a) SELECT a FROM d",
            "permission denied for table qp_cte_t",
        ),
        (
            "WITH i AS (INSERT INTO qp_cte_t (a) VALUES (1) RETURNING id) SELECT id FROM i",
            "permission denied for table qp_cte_t",
        ),
        (
            "WITH x AS (SELECT a FROM qp_cte_t) SELECT a FROM x",
            "permission denied for table qp_cte_t",
        ),
    ] {
        denied_with(&db, "qp_cte_app", sql, prefix);
    }
}

#[test]
fn sublinks_set_operations_and_lateral() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_sub_app;
         CREATE TABLE qp_sub_t (id int, a int);
         CREATE TABLE qp_sub_hidden (id int, b int);
         GRANT SELECT (id) ON qp_sub_t TO qp_sub_app;",
    )]);
    for sql in [
        "SELECT id FROM qp_sub_t WHERE EXISTS (SELECT 1 FROM qp_sub_t u WHERE u.id = qp_sub_t.id)",
        "SELECT id FROM qp_sub_t WHERE id IN (SELECT id FROM qp_sub_t u)",
        "SELECT (SELECT max(u.id) FROM qp_sub_t u) FROM qp_sub_t",
        "SELECT id FROM qp_sub_t UNION SELECT id FROM qp_sub_t",
        "SELECT id FROM qp_sub_t INTERSECT SELECT id FROM qp_sub_t",
        "SELECT t.id FROM qp_sub_t t, LATERAL (SELECT u.id FROM qp_sub_t u WHERE u.id = t.id) s",
    ] {
        allowed(&db, "qp_sub_app", sql);
    }
    for (sql, prefix) in [
        (
            "SELECT id FROM qp_sub_t WHERE EXISTS (SELECT 1 FROM qp_sub_hidden h)",
            "permission denied for table qp_sub_hidden",
        ),
        (
            "SELECT id FROM qp_sub_t WHERE id IN (SELECT b FROM qp_sub_hidden)",
            "permission denied for table qp_sub_hidden",
        ),
        (
            "SELECT (SELECT b FROM qp_sub_hidden) FROM qp_sub_t",
            "permission denied for table qp_sub_hidden",
        ),
        (
            "SELECT id FROM qp_sub_t UNION SELECT b FROM qp_sub_hidden",
            "permission denied for table qp_sub_hidden",
        ),
        (
            "SELECT t.id FROM qp_sub_t t, LATERAL (SELECT h.b FROM qp_sub_hidden h WHERE h.id = t.id) s",
            "permission denied for table qp_sub_hidden",
        ),
        (
            "SELECT id FROM qp_sub_t WHERE id IN (SELECT a FROM qp_sub_t)",
            "permission denied for table qp_sub_t",
        ),
    ] {
        denied_with(&db, "qp_sub_app", sql, prefix);
    }
}

#[test]
fn views_read_as_their_owner_unless_security_invoker() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_view_app;
         CREATE ROLE qp_view_owner;
         CREATE ROLE qp_view_col;
         CREATE ROLE qp_view_inv_app;
         CREATE SCHEMA qp_view_s AUTHORIZATION qp_view_owner;
         CREATE TABLE public.qp_view_secret (id int, a int);
         CREATE VIEW public.qp_view_v AS SELECT id, a FROM public.qp_view_secret;
         CREATE VIEW qp_view_s.v2 AS SELECT id, a FROM public.qp_view_secret;
         ALTER VIEW qp_view_s.v2 OWNER TO qp_view_owner;
         CREATE VIEW public.qp_view_inv WITH (security_invoker = true)
             AS SELECT id, a FROM public.qp_view_secret;
         CREATE VIEW public.qp_view_nested AS SELECT id FROM public.qp_view_v;
         CREATE VIEW public.qp_view_nested_restricted AS SELECT id FROM qp_view_s.v2;
         GRANT USAGE ON SCHEMA qp_view_s TO qp_view_app, qp_view_inv_app;
         GRANT SELECT ON qp_view_s.v2 TO qp_view_app;
         GRANT SELECT ON public.qp_view_v, public.qp_view_inv, public.qp_view_nested,
                          public.qp_view_nested_restricted TO qp_view_app;
         GRANT SELECT (id) ON public.qp_view_v TO qp_view_col;
         GRANT SELECT ON public.qp_view_inv, public.qp_view_secret TO qp_view_inv_app;
         GRANT SELECT ON qp_view_s.v2 TO qp_view_inv_app;",
    )]);
    // The view's query reads the base table as the view's owner (the
    // migrations' user), so SELECT on the view alone suffices.
    allowed(&db, "qp_view_app", "SELECT * FROM qp_view_v");
    allowed(&db, "qp_view_app", "SELECT * FROM qp_view_nested");
    denied_with(
        &db,
        "qp_view_app",
        "SELECT * FROM qp_view_secret",
        "permission denied for table qp_view_secret",
    );
    // A view owned by a role without privileges on its base table denies
    // with the base table's error.
    denied_with(
        &db,
        "qp_view_app",
        "SELECT * FROM qp_view_s.v2",
        "permission denied for table qp_view_secret",
    );
    denied_with(
        &db,
        "qp_view_app",
        "SELECT * FROM qp_view_nested_restricted",
        "permission denied for table qp_view_secret",
    );
    // security_invoker reads as the caller.
    denied_with(
        &db,
        "qp_view_app",
        "SELECT * FROM qp_view_inv",
        "permission denied for table qp_view_secret",
    );
    allowed(&db, "qp_view_inv_app", "SELECT * FROM qp_view_inv");
    denied_with(
        &db,
        "qp_view_inv_app",
        "SELECT * FROM qp_view_s.v2",
        "permission denied for table qp_view_secret",
    );
    // Column privileges on the view are the caller's; the base table is
    // the owner's.
    allowed(&db, "qp_view_col", "SELECT id FROM qp_view_v");
    denied_with(
        &db,
        "qp_view_col",
        "SELECT a FROM qp_view_v",
        "permission denied for view qp_view_v",
    );
    denied_with(
        &db,
        "qp_view_col",
        "SELECT qp_view_v FROM qp_view_v",
        "permission denied for view qp_view_v",
    );
}

#[test]
fn view_functions_run_as_the_caller() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vf_app;
         CREATE ROLE qp_vf_granted;
         CREATE FUNCTION qp_vf_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         CREATE VIEW qp_vf_v AS SELECT qp_vf_f() AS x;
         REVOKE EXECUTE ON FUNCTION qp_vf_f() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_vf_f() TO qp_vf_granted;
         GRANT SELECT ON qp_vf_v TO qp_vf_app, qp_vf_granted;",
    )]);
    denied_with(
        &db,
        "qp_vf_app",
        "SELECT x FROM qp_vf_v",
        "permission denied for function qp_vf_f",
    );
    allowed(&db, "qp_vf_granted", "SELECT x FROM qp_vf_v");
}

#[test]
fn auto_updatable_views_write_their_base_as_the_owner() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vd_app;
         CREATE ROLE qp_vd_baseowner;
         CREATE ROLE qp_vd_read;
         CREATE ROLE qp_vd_baseonly;
         CREATE SCHEMA qp_vd_s AUTHORIZATION qp_vd_baseowner;
         CREATE TABLE public.qp_vd_b (id int, a int);
         CREATE VIEW public.qp_vd_v AS SELECT id, a FROM public.qp_vd_b;
         GRANT SELECT, INSERT, UPDATE, DELETE ON public.qp_vd_v TO qp_vd_app;
         CREATE TABLE public.qp_vd_b2 (id int, a int);
         CREATE VIEW qp_vd_s.v2 AS SELECT id, a FROM public.qp_vd_b2;
         ALTER VIEW qp_vd_s.v2 OWNER TO qp_vd_baseowner;
         GRANT USAGE ON SCHEMA qp_vd_s TO qp_vd_app;
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_vd_s.v2 TO qp_vd_app;
         GRANT SELECT ON public.qp_vd_v TO qp_vd_read;
         GRANT INSERT, SELECT (id) ON public.qp_vd_b TO qp_vd_baseonly;",
    )]);
    // The caller needs the write privilege on the view only; the base
    // table is written as the view's owner (the migrations' user).
    for sql in [
        "INSERT INTO qp_vd_v (id, a) VALUES (1, 1)",
        "INSERT INTO qp_vd_v (id, a) VALUES (1, 1) RETURNING id",
        "UPDATE qp_vd_v SET a = 1 WHERE id = 1",
        "UPDATE qp_vd_v SET a = 1 WHERE id = 1 RETURNING a",
        "DELETE FROM qp_vd_v WHERE id = 1",
        "DELETE FROM qp_vd_v RETURNING a",
    ] {
        allowed(&db, "qp_vd_app", sql);
    }
    // The base table's privileges do not replace the view's.
    denied_with(
        &db,
        "qp_vd_baseonly",
        "INSERT INTO qp_vd_v (id, a) VALUES (1, 1)",
        "permission denied for view qp_vd_v",
    );
    denied_with(
        &db,
        "qp_vd_read",
        "INSERT INTO qp_vd_v (id, a) VALUES (1, 1)",
        "permission denied for view qp_vd_v",
    );
    // A view owner without privileges on the base table cannot write it.
    denied_with(
        &db,
        "qp_vd_app",
        "INSERT INTO qp_vd_s.v2 (id, a) VALUES (1, 1)",
        "permission denied for table qp_vd_b2",
    );
    denied_with(
        &db,
        "qp_vd_app",
        "UPDATE qp_vd_s.v2 SET a = 1 WHERE id = 1",
        "permission denied for table qp_vd_b2",
    );
    denied_with(
        &db,
        "qp_vd_app",
        "DELETE FROM qp_vd_s.v2 WHERE id = 1",
        "permission denied for table qp_vd_b2",
    );
}

#[test]
fn function_and_operator_execute() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_rt_app;
         CREATE ROLE qp_rt_granted;
         CREATE FUNCTION qp_rt_f(int) RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN $1; END $$;
         CREATE FUNCTION qp_rt_srf() RETURNS SETOF int LANGUAGE plpgsql
             AS $$ BEGIN RETURN QUERY SELECT 1; END $$;
         CREATE FUNCTION qp_rt_opfn(int, int) RETURNS bool LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         CREATE OPERATOR public.=== (leftarg = int, rightarg = int, function = qp_rt_opfn);
         CREATE FUNCTION qp_rt_df() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         CREATE TABLE qp_rt_t (a int DEFAULT qp_rt_df());
         CREATE FUNCTION qp_rt_pd(a int DEFAULT qp_rt_df()) RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN a; END $$;
         CREATE FUNCTION qp_rt_gf() RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN 1; END $$;
         CREATE TABLE qp_rt_g (a int, g int GENERATED ALWAYS AS (qp_rt_gf()) STORED);
         REVOKE EXECUTE ON FUNCTION qp_rt_f(int) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_rt_srf() FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_rt_opfn(int, int) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_rt_df() FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_rt_pd(int) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_rt_gf() FROM PUBLIC;
         GRANT INSERT (a) ON qp_rt_t TO qp_rt_app, qp_rt_granted;
         GRANT INSERT (a) ON qp_rt_g TO qp_rt_app, qp_rt_granted;
         GRANT EXECUTE ON FUNCTION qp_rt_pd(int) TO qp_rt_app;
         GRANT EXECUTE ON FUNCTION qp_rt_f(int) TO qp_rt_granted;
         GRANT EXECUTE ON FUNCTION qp_rt_srf() TO qp_rt_granted;
         GRANT EXECUTE ON FUNCTION qp_rt_opfn(int, int) TO qp_rt_granted;
         GRANT EXECUTE ON FUNCTION qp_rt_df() TO qp_rt_granted;
         GRANT EXECUTE ON FUNCTION qp_rt_gf() TO qp_rt_granted;
         GRANT EXECUTE ON FUNCTION qp_rt_pd(int) TO qp_rt_granted;",
    )]);
    for (sql, prefix) in [
        (
            "SELECT qp_rt_f(1)",
            "permission denied for function qp_rt_f",
        ),
        (
            "SELECT * FROM qp_rt_srf()",
            "permission denied for function qp_rt_srf",
        ),
        (
            "SELECT 1 === 2",
            "permission denied for function qp_rt_opfn",
        ),
        // The column default the INSERT fills runs with the statement.
        (
            "INSERT INTO qp_rt_t (a) VALUES (DEFAULT)",
            "permission denied for function qp_rt_df",
        ),
        // A parameter's DEFAULT expression runs with the call.
        (
            "SELECT qp_rt_pd()",
            "permission denied for function qp_rt_df",
        ),
        // A generated column's expression runs with the INSERT.
        (
            "INSERT INTO qp_rt_g (a) VALUES (1)",
            "permission denied for function qp_rt_gf",
        ),
    ] {
        denied_with(&db, "qp_rt_app", sql, prefix);
    }
    // The function itself is granted; its parameter default is not: both
    // are checked, so the default's function is still needed.
    allowed(&db, "qp_rt_granted", "SELECT qp_rt_f(1)");
    allowed(&db, "qp_rt_granted", "SELECT * FROM qp_rt_srf()");
    allowed(&db, "qp_rt_granted", "SELECT 1 === 2");
    allowed(
        &db,
        "qp_rt_granted",
        "INSERT INTO qp_rt_t (a) VALUES (DEFAULT)",
    );
    allowed(&db, "qp_rt_granted", "SELECT qp_rt_pd()");
    allowed(&db, "qp_rt_granted", "INSERT INTO qp_rt_g (a) VALUES (1)");
}

#[test]
fn instead_of_trigger_views_need_only_the_view_privilege() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_it_app;
         CREATE ROLE qp_it_owner;
         CREATE ROLE qp_it_ins_owner;
         CREATE SCHEMA qp_it_s AUTHORIZATION qp_it_owner;
         CREATE TABLE public.qp_it_b (id int, a int);
         CREATE TABLE public.qp_it_b2 (id int, a int);
         CREATE VIEW qp_it_s.v AS SELECT id, a FROM public.qp_it_b;
         ALTER VIEW qp_it_s.v OWNER TO qp_it_owner;
         CREATE VIEW qp_it_s.vi AS SELECT id, a FROM public.qp_it_b2;
         ALTER VIEW qp_it_s.vi OWNER TO qp_it_ins_owner;
         -- Updating or deleting scans the view, so its owner must read
         -- the base table; inserting does not.
         GRANT SELECT ON public.qp_it_b TO qp_it_owner;
         CREATE FUNCTION qp_it_tf() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN RETURN NEW; END $$;
         CREATE TRIGGER qp_it_ti INSTEAD OF INSERT ON qp_it_s.v
             FOR EACH ROW EXECUTE FUNCTION qp_it_tf();
         CREATE TRIGGER qp_it_tu INSTEAD OF UPDATE ON qp_it_s.v
             FOR EACH ROW EXECUTE FUNCTION qp_it_tf();
         CREATE TRIGGER qp_it_td INSTEAD OF DELETE ON qp_it_s.v
             FOR EACH ROW EXECUTE FUNCTION qp_it_tf();
         CREATE TRIGGER qp_it_vi_ti INSTEAD OF INSERT ON qp_it_s.vi
             FOR EACH ROW EXECUTE FUNCTION qp_it_tf();
         GRANT USAGE ON SCHEMA qp_it_s TO qp_it_app;
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_it_s.v TO qp_it_app;
         GRANT SELECT, INSERT, UPDATE ON qp_it_s.vi TO qp_it_app;",
    )]);
    // The trigger handles the write: the base table is never written, so
    // its owner's INSERT/UPDATE/DELETE privileges play no part.
    for sql in [
        "INSERT INTO qp_it_s.v (id, a) VALUES (1, 1)",
        "UPDATE qp_it_s.v SET a = 1 WHERE id = 1",
        "DELETE FROM qp_it_s.v WHERE id = 1",
    ] {
        allowed(&db, "qp_it_app", sql);
    }
    // An INSERT-only trigger view never reads its base table either, so an
    // owner with no privileges on it is enough.
    allowed(
        &db,
        "qp_it_app",
        "INSERT INTO qp_it_s.vi (id, a) VALUES (1, 1)",
    );
    // Without an UPDATE trigger the view is auto-updatable: its base table
    // takes the write as the view's owner, which lacks UPDATE there.
    denied_with(
        &db,
        "qp_it_app",
        "UPDATE qp_it_s.vi SET a = 1 WHERE id = 1",
        "permission denied for table qp_it_b2",
    );
}

#[test]
fn cast_functions_need_execute() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_cast_app;
         CREATE ROLE qp_cast_granted;
         CREATE TYPE qp_cast_c AS (x int);
         CREATE FUNCTION qp_cast_f(int) RETURNS qp_cast_c LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN ROW($1)::qp_cast_c; END $$;
         CREATE CAST (int AS qp_cast_c) WITH FUNCTION qp_cast_f(int) AS ASSIGNMENT;
         CREATE TABLE qp_cast_t (c qp_cast_c);
         GRANT USAGE ON TYPE qp_cast_c TO qp_cast_app, qp_cast_granted;
         GRANT INSERT (c) ON qp_cast_t TO qp_cast_app, qp_cast_granted;
         REVOKE EXECUTE ON FUNCTION qp_cast_f(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_cast_f(int) TO qp_cast_granted;",
    )]);
    // The cast is a call of its function, explicit or assignment.
    denied_with(
        &db,
        "qp_cast_app",
        "SELECT 1::qp_cast_c",
        "permission denied for function qp_cast_f",
    );
    denied_with(
        &db,
        "qp_cast_app",
        "INSERT INTO qp_cast_t (c) VALUES (1)",
        "permission denied for function qp_cast_f",
    );
    allowed(&db, "qp_cast_granted", "SELECT 1::qp_cast_c");
    allowed(
        &db,
        "qp_cast_granted",
        "INSERT INTO qp_cast_t (c) VALUES (1)",
    );
}

#[test]
fn aggregate_window_and_procedure_execute() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_agg_app;
         CREATE ROLE qp_agg_granted;
         CREATE TABLE qp_agg_t (a int);
         INSERT INTO qp_agg_t VALUES (1);
         CREATE AGGREGATE qp_agg_sum(int) (sfunc = int4pl, stype = int);
         CREATE PROCEDURE qp_agg_p() LANGUAGE plpgsql AS $$ BEGIN END $$;
         REVOKE EXECUTE ON FUNCTION qp_agg_sum(int) FROM PUBLIC;
         REVOKE EXECUTE ON PROCEDURE qp_agg_p() FROM PUBLIC;
         GRANT SELECT ON qp_agg_t TO qp_agg_app, qp_agg_granted;
         GRANT EXECUTE ON FUNCTION qp_agg_sum(int) TO qp_agg_granted;
         GRANT EXECUTE ON PROCEDURE qp_agg_p() TO qp_agg_granted;",
    )]);
    denied_with(
        &db,
        "qp_agg_app",
        "SELECT qp_agg_sum(a) FROM qp_agg_t",
        "permission denied for aggregate qp_agg_sum",
    );
    // As a window function, PG names the same routine a function.
    denied_with(
        &db,
        "qp_agg_app",
        "SELECT qp_agg_sum(a) OVER () FROM qp_agg_t",
        "permission denied for function qp_agg_sum",
    );
    denied_with(
        &db,
        "qp_agg_app",
        "CALL qp_agg_p()",
        "permission denied for procedure qp_agg_p",
    );
    allowed(&db, "qp_agg_granted", "SELECT qp_agg_sum(a) FROM qp_agg_t");
    allowed(
        &db,
        "qp_agg_granted",
        "SELECT qp_agg_sum(a) OVER () FROM qp_agg_t",
    );
    allowed(&db, "qp_agg_granted", "CALL qp_agg_p()");
}

#[test]
fn schema_usage_is_checked_for_explicit_names() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_sch_app;
         CREATE ROLE qp_sch_granted;
         CREATE SCHEMA qp_sch_s;
         CREATE TABLE qp_sch_s.t (id int);
         CREATE SEQUENCE qp_sch_s.seq;
         CREATE FUNCTION qp_sch_s.f() RETURNS int LANGUAGE sql AS 'SELECT 1';
         GRANT SELECT ON qp_sch_s.t TO qp_sch_app, qp_sch_granted;
         GRANT USAGE ON SCHEMA qp_sch_s TO qp_sch_granted;
         GRANT EXECUTE ON FUNCTION qp_sch_s.f() TO qp_sch_app;",
    )]);
    denied_with(
        &db,
        "qp_sch_app",
        "SELECT * FROM qp_sch_s.t",
        "permission denied for schema qp_sch_s",
    );
    denied_with(
        &db,
        "qp_sch_app",
        "SELECT qp_sch_s.f()",
        "permission denied for schema qp_sch_s",
    );
    denied_with(
        &db,
        "qp_sch_app",
        "SELECT nextval('qp_sch_s.seq')",
        "permission denied for schema qp_sch_s",
    );
    allowed(&db, "qp_sch_granted", "SELECT * FROM qp_sch_s.t");
}

#[test]
fn role_search_path_decides_unqualified_names() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_path_app;
         CREATE ROLE qp_path_missing;
         CREATE ROLE qp_path_nousage;
         CREATE ROLE qp_path_user;
         CREATE SCHEMA qp_path_s;
         CREATE TABLE qp_path_s.t (id int);
         GRANT USAGE ON SCHEMA qp_path_s TO qp_path_app;
         GRANT SELECT ON qp_path_s.t TO qp_path_app;
         ALTER ROLE qp_path_app SET search_path = qp_path_s, public;
         ALTER ROLE qp_path_missing SET search_path = qp_path_s, public;
         ALTER ROLE qp_path_nousage SET search_path = qp_path_s, public;
         CREATE SCHEMA qp_path_user AUTHORIZATION qp_path_user;
         CREATE TABLE qp_path_user.t (id int);
         GRANT USAGE ON SCHEMA qp_path_user TO qp_path_user;
         GRANT SELECT ON qp_path_user.t TO qp_path_user;
         ALTER ROLE qp_path_user SET search_path = \"$user\", public;",
    )]);
    // The role's own search_path resolves the name.
    allowed(&db, "qp_path_app", "SELECT * FROM t");
    // A schema the role lacks USAGE on is not on its search path: the
    // name is not found at all.
    let e = denied(&db, "qp_path_missing", "SELECT * FROM t");
    assert!(e.starts_with("relation \"t\" does not exist"), "{e}");
    let e = denied(&db, "qp_path_nousage", "SELECT * FROM t");
    assert!(e.starts_with("relation \"t\" does not exist"), "{e}");
    // `"$user"` names the schema called like the role.
    allowed(&db, "qp_path_user", "SELECT * FROM t");
    // Roles without a setting keep the default search path.
    let e = denied(&db, "qp_path_user", "SELECT * FROM qp_path_s.t");
    assert!(
        e.starts_with("permission denied for schema qp_path_s"),
        "{e}"
    );
}

#[test]
fn role_membership_and_public_grants() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_mem_owner;
         CREATE ROLE qp_mem_app;
         CREATE ROLE qp_mem_noinherit NOINHERIT;
         CREATE ROLE qp_mem_noinherit2 NOINHERIT;
         CREATE ROLE qp_mem_explicit;
         CREATE ROLE qp_mem_chain1;
         CREATE ROLE qp_mem_chain2;
         CREATE ROLE qp_mem_public;
         CREATE TABLE qp_mem_t (id int);
         CREATE TABLE qp_mem_pub (id int);
         GRANT SELECT ON qp_mem_t TO qp_mem_owner;
         GRANT qp_mem_owner TO qp_mem_app;
         GRANT qp_mem_owner TO qp_mem_noinherit;
         GRANT qp_mem_owner TO qp_mem_noinherit2 WITH INHERIT TRUE;
         GRANT qp_mem_owner TO qp_mem_explicit WITH INHERIT FALSE;
         GRANT qp_mem_owner TO qp_mem_chain1;
         GRANT qp_mem_chain1 TO qp_mem_chain2;
         GRANT SELECT ON qp_mem_pub TO PUBLIC;",
    )]);
    allowed(&db, "qp_mem_app", "SELECT * FROM qp_mem_t");
    allowed(&db, "qp_mem_chain2", "SELECT * FROM qp_mem_t");
    allowed(&db, "qp_mem_noinherit2", "SELECT * FROM qp_mem_t");
    denied_with(
        &db,
        "qp_mem_noinherit",
        "SELECT * FROM qp_mem_t",
        "permission denied for table qp_mem_t",
    );
    denied_with(
        &db,
        "qp_mem_explicit",
        "SELECT * FROM qp_mem_t",
        "permission denied for table qp_mem_t",
    );
    allowed(&db, "qp_mem_public", "SELECT * FROM qp_mem_pub");
    denied_with(
        &db,
        "qp_mem_public",
        "SELECT * FROM qp_mem_t",
        "permission denied for table qp_mem_t",
    );
}

#[test]
fn predefined_roles_and_superusers() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_pre_read;
         CREATE ROLE qp_pre_write;
         CREATE ROLE qp_pre_super SUPERUSER;
         CREATE TABLE qp_pre_t (id int);
         GRANT pg_read_all_data TO qp_pre_read;
         GRANT pg_write_all_data TO qp_pre_write;",
    )]);
    allowed(&db, "qp_pre_read", "SELECT * FROM qp_pre_t");
    denied_with(
        &db,
        "qp_pre_read",
        "INSERT INTO qp_pre_t (id) VALUES (1)",
        "permission denied for table qp_pre_t",
    );
    allowed(&db, "qp_pre_write", "INSERT INTO qp_pre_t (id) VALUES (1)");
    allowed(&db, "qp_pre_write", "UPDATE qp_pre_t SET id = 1");
    allowed(&db, "qp_pre_write", "DELETE FROM qp_pre_t");
    denied_with(
        &db,
        "qp_pre_write",
        "SELECT * FROM qp_pre_t",
        "permission denied for table qp_pre_t",
    );
    allowed(&db, "qp_pre_super", "SELECT * FROM qp_pre_t");
    allowed(&db, "qp_pre_super", "INSERT INTO qp_pre_t (id) VALUES (1)");
    allowed(
        &db,
        "qp_pre_super",
        "UPDATE qp_pre_t SET id = 1 WHERE id = 1",
    );
    allowed(&db, "qp_pre_super", "DELETE FROM qp_pre_t");
}

#[test]
fn unknown_role_is_reported() {
    let db = build_db(&[("0001.sql", "CREATE TABLE qp_unknown_t (id int);")]);
    denied_with(
        &db,
        "qp_unknown_missing_role",
        "SELECT 1",
        "role \"qp_unknown_missing_role\" does not exist",
    );
}

#[test]
fn rendered_error_names_the_role_what_is_missing_and_the_grant() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_full_app;
         CREATE ROLE qp_full_none;
         CREATE TABLE qp_full_t (id int, b int);
         GRANT SELECT (id) ON qp_full_t TO qp_full_app;",
    )]);
    // A missing table privilege.
    let e = denied(&db, "qp_full_none", "SELECT count(*) FROM qp_full_t");
    assert_eq!(
        e,
        "permission denied for table qp_full_t\n  \
         help: grant it in a migration: GRANT SELECT ON public.qp_full_t TO qp_full_none;\n  \
         note: role qp_full_none has no SELECT on public.qp_full_t\n"
    );
    // A missing column privilege names the columns.
    let e = denied(&db, "qp_full_app", "SELECT b FROM qp_full_t");
    assert_eq!(
        e,
        "permission denied for table qp_full_t\n  \
         help: grant it in a migration: GRANT SELECT (b) ON public.qp_full_t TO qp_full_app;\n  \
         note: role qp_full_app has no SELECT on column(s) b of public.qp_full_t\n"
    );
    let e = denied(&db, "qp_full_app", "SELECT * FROM qp_full_t");
    assert_eq!(
        e,
        "permission denied for table qp_full_t\n  \
         help: grant it in a migration: GRANT SELECT (b) ON public.qp_full_t TO qp_full_app;\n  \
         note: role qp_full_app has no SELECT on column(s) b of public.qp_full_t\n"
    );
}

#[test]
fn analyze_as_with_named_parameters_and_spreads() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_par_app;
         CREATE TABLE qp_par_t (a int, b int);
         GRANT SELECT (a), INSERT (a), UPDATE (a) ON qp_par_t TO qp_par_app;",
    )]);
    allowed(
        &db,
        "qp_par_app",
        "SELECT a FROM qp_par_t WHERE a = $wanted",
    );
    denied_with(
        &db,
        "qp_par_app",
        "SELECT b FROM qp_par_t WHERE a = $wanted",
        "permission denied for table qp_par_t",
    );
    allowed(
        &db,
        "qp_par_app",
        "INSERT INTO qp_par_t (a) VALUES $..rows { a }",
    );
    denied_with(
        &db,
        "qp_par_app",
        "INSERT INTO qp_par_t (a) VALUES $..rows { a } RETURNING b",
        "permission denied for table qp_par_t",
    );
    denied_with(
        &db,
        "qp_par_app",
        "INSERT INTO qp_par_t (b) VALUES $..rows { b }",
        "permission denied for table qp_par_t",
    );
    allowed(
        &db,
        "qp_par_app",
        "UPDATE qp_par_t SET a = $value WHERE a = $key",
    );
}

#[test]
fn row_locking_reaches_into_from_subqueries() {
    // transformLockingClause pushes FOR UPDATE / SHARE into a FROM
    // subquery: its relations need UPDATE too.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_locksub_app;
         CREATE TABLE u (id int, a int);
         CREATE TABLE w (id int, b int);
         GRANT SELECT ON u, w TO qp_locksub_app;",
    )]);
    let e = denied(
        &db,
        "qp_locksub_app",
        "SELECT * FROM (SELECT id FROM u) s FOR UPDATE",
    );
    assert!(e.starts_with("permission denied for table u"), "{e}");
    denied(
        &db,
        "qp_locksub_app",
        "SELECT * FROM (SELECT * FROM (SELECT id FROM u) s1) s2 FOR SHARE",
    );
    denied(
        &db,
        "qp_locksub_app",
        "SELECT * FROM w, (SELECT u.id FROM u JOIN u u2 ON u2.id = u.id) s FOR UPDATE OF s",
    );
    // OF another entry: the subquery isn't locked.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_locksub_of_app;
         CREATE TABLE u (id int, a int);
         CREATE TABLE w (id int, b int);
         GRANT SELECT ON u, w TO qp_locksub_of_app;
         GRANT UPDATE (b) ON w TO qp_locksub_of_app;",
    )]);
    allowed(
        &db,
        "qp_locksub_of_app",
        "SELECT * FROM w, (SELECT id FROM u) s FOR UPDATE OF w",
    );
    // Any updatable column makes the lock allowed.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_locksub_col_app;
         CREATE TABLE u (id int, a int);
         GRANT SELECT ON u TO qp_locksub_col_app;
         GRANT UPDATE (a) ON u TO qp_locksub_col_app;",
    )]);
    allowed(
        &db,
        "qp_locksub_col_app",
        "SELECT * FROM (SELECT id FROM u) s FOR UPDATE",
    );
}

#[test]
fn writes_through_a_view_run_its_query_only_when_pg_does() {
    // The view's owner may write the base table but not read it. An
    // INSERT, UPDATE or DELETE the rewriter turns into one of the base
    // table (rewriteTargetView) doesn't run the view's query; reading the
    // view, or locking it, does.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vwrite_owner;
         CREATE ROLE qp_vwrite_app;
         CREATE TABLE t (id int, a int);
         GRANT INSERT, UPDATE, DELETE ON t TO qp_vwrite_owner;
         CREATE VIEW v AS SELECT id, a FROM t;
         ALTER VIEW v OWNER TO qp_vwrite_owner;
         GRANT SELECT, INSERT, UPDATE, DELETE ON v TO qp_vwrite_app;",
    )]);
    allowed(&db, "qp_vwrite_app", "INSERT INTO v (id, a) VALUES (1, 2)");
    allowed(&db, "qp_vwrite_app", "UPDATE v SET a = 1");
    allowed(&db, "qp_vwrite_app", "DELETE FROM v");
    let e = denied(&db, "qp_vwrite_app", "SELECT id FROM v");
    assert!(e.starts_with("permission denied for table t"), "{e}");
    denied(&db, "qp_vwrite_app", "UPDATE v SET a = 1 WHERE id = 1");
}

// ─── Routine bodies (#269, step 4) ─────────────────────────────────────────

#[test]
fn trigger_body_writes_audit_table() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_trg_audit_app;
         CREATE TABLE qp_trg_audit_t (id int, a int);
         CREATE TABLE qp_trg_audit (id int, a int);
         INSERT INTO qp_trg_audit_t VALUES (1, 1);
         CREATE FUNCTION qp_trg_audit_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_trg_audit (id, a) VALUES (NEW.id, NEW.a);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_trg_audit_trg AFTER INSERT ON qp_trg_audit_t
           FOR EACH ROW EXECUTE FUNCTION qp_trg_audit_fn();
         GRANT SELECT, INSERT ON qp_trg_audit_t TO qp_trg_audit_app;",
    )]);
    let e = denied_with(
        &db,
        "qp_trg_audit_app",
        "INSERT INTO qp_trg_audit_t (id, a) VALUES (2, 2)",
        "permission denied for table qp_trg_audit",
    );
    assert!(
        e.contains("in trigger function public.qp_trg_audit_fn(), fired by INSERT on public.qp_trg_audit_t, run as role qp_trg_audit_app"),
        "{e}"
    );

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_trg_audit_ok_app;
         CREATE TABLE qp_trg_audit_ok_t (id int, a int);
         CREATE TABLE qp_trg_audit_ok (id int, a int);
         CREATE FUNCTION qp_trg_audit_ok_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_trg_audit_ok (id, a) VALUES (NEW.id, NEW.a);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_trg_audit_ok_trg BEFORE INSERT ON qp_trg_audit_ok_t
           FOR EACH ROW EXECUTE FUNCTION qp_trg_audit_ok_fn();
         GRANT SELECT, INSERT ON qp_trg_audit_ok_t TO qp_trg_audit_ok_app;
         GRANT INSERT ON qp_trg_audit_ok TO qp_trg_audit_ok_app;",
    )]);
    allowed(
        &db,
        "qp_trg_audit_ok_app",
        "INSERT INTO qp_trg_audit_ok_t (id, a) VALUES (2, 2)",
    );
}

#[test]
fn security_definer_trigger_runs_as_its_owner() {
    // Owned by the migrations' user (a superuser-equivalent): the body may
    // write the audit table whatever the role.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_def_ok_app;
         CREATE TABLE qp_def_ok_t (id int, a int);
         CREATE TABLE qp_def_ok_audit (id int);
         CREATE FUNCTION qp_def_ok_fn() RETURNS trigger LANGUAGE plpgsql
             SECURITY DEFINER AS $$
         BEGIN
           INSERT INTO qp_def_ok_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_def_ok_trg AFTER INSERT ON qp_def_ok_t
             FOR EACH ROW EXECUTE FUNCTION qp_def_ok_fn();
         GRANT SELECT, INSERT ON qp_def_ok_t TO qp_def_ok_app;",
    )]);
    allowed(
        &db,
        "qp_def_ok_app",
        "INSERT INTO qp_def_ok_t (id, a) VALUES (1, 1)",
    );

    // Owned by a role without INSERT on the audit table: the body is denied
    // as that owner, and the note says so.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_def_bad_app;
         CREATE ROLE qp_def_bad_owner;
         CREATE TABLE qp_def_bad_t (id int, a int);
         CREATE TABLE qp_def_bad_audit (id int);
         CREATE FUNCTION qp_def_bad_fn() RETURNS trigger LANGUAGE plpgsql
             SECURITY DEFINER AS $$
         BEGIN
           INSERT INTO qp_def_bad_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         ALTER FUNCTION qp_def_bad_fn() OWNER TO qp_def_bad_owner;
         CREATE TRIGGER qp_def_bad_trg AFTER INSERT ON qp_def_bad_t
             FOR EACH ROW EXECUTE FUNCTION qp_def_bad_fn();
         GRANT SELECT, INSERT ON qp_def_bad_t TO qp_def_bad_app;",
    )]);
    let e = denied_with(
        &db,
        "qp_def_bad_app",
        "INSERT INTO qp_def_bad_t (id, a) VALUES (1, 1)",
        "permission denied for table qp_def_bad_audit",
    );
    assert!(
        e.contains(
            "in trigger function public.qp_def_bad_fn(), fired by INSERT on public.qp_def_bad_t, \
             run as its owner qp_def_bad_owner (SECURITY DEFINER)"
        ),
        "{e}"
    );
    // The statement itself is still the role's: dropping the trigger makes
    // it allowed.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_def_bad2_app;
         CREATE TABLE qp_def_bad2_t (id int, a int);
         GRANT SELECT, INSERT ON qp_def_bad2_t TO qp_def_bad2_app;",
    )]);
    allowed(
        &db,
        "qp_def_bad2_app",
        "INSERT INTO qp_def_bad2_t (id, a) VALUES (1, 1)",
    );
}

#[test]
fn statement_level_update_trigger_runs_even_for_no_rows() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_stmt_app;
         CREATE TABLE qp_stmt_t (id int, a int);
         CREATE TABLE qp_stmt_audit (n int);
         CREATE FUNCTION qp_stmt_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_stmt_audit (n) VALUES (1);
           RETURN NULL;
         END $$;
         CREATE TRIGGER qp_stmt_trg AFTER UPDATE ON qp_stmt_t
             FOR EACH STATEMENT EXECUTE FUNCTION qp_stmt_fn();
         GRANT SELECT, UPDATE ON qp_stmt_t TO qp_stmt_app;",
    )]);
    // The table is empty and the WHERE matches nothing; a statement trigger
    // fires anyway.
    denied_with(
        &db,
        "qp_stmt_app",
        "UPDATE qp_stmt_t SET a = 1 WHERE false",
        "permission denied for table qp_stmt_audit",
    );
}

#[test]
fn update_of_trigger_fires_only_for_named_columns() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_updof_app;
         CREATE TABLE qp_updof_t (id int, a int, b int);
         INSERT INTO qp_updof_t VALUES (1, 1, 1);
         CREATE TABLE qp_updof_audit (id int);
         CREATE FUNCTION qp_updof_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_updof_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_updof_trg BEFORE UPDATE OF b ON qp_updof_t
             FOR EACH ROW EXECUTE FUNCTION qp_updof_fn();
         GRANT SELECT, UPDATE (a, b) ON qp_updof_t TO qp_updof_app;",
    )]);
    allowed(&db, "qp_updof_app", "UPDATE qp_updof_t SET a = 2");
    denied_with(
        &db,
        "qp_updof_app",
        "UPDATE qp_updof_t SET b = 2",
        "permission denied for table qp_updof_audit",
    );
}

#[test]
fn delete_trigger_runs() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_deltrg_app;
         CREATE TABLE qp_del_t (id int, a int);
         INSERT INTO qp_del_t VALUES (1, 1);
         CREATE TABLE qp_del_audit (id int);
         CREATE FUNCTION qp_del_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_del_audit (id) VALUES (OLD.id);
           RETURN OLD;
         END $$;
         CREATE TRIGGER qp_del_trg AFTER DELETE ON qp_del_t
             FOR EACH ROW EXECUTE FUNCTION qp_del_fn();
         GRANT SELECT, DELETE ON qp_del_t TO qp_deltrg_app;",
    )]);
    denied_with(
        &db,
        "qp_deltrg_app",
        "DELETE FROM qp_del_t",
        "permission denied for table qp_del_audit",
    );
}

#[test]
fn on_conflict_do_update_fires_update_row_triggers() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_oc_app;
         CREATE TABLE qp_oc_t (id int PRIMARY KEY, a int);
         INSERT INTO qp_oc_t VALUES (1, 1);
         CREATE TABLE qp_oc_audit (id int);
         CREATE FUNCTION qp_oc_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_oc_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_oc_trg AFTER UPDATE ON qp_oc_t
             FOR EACH ROW EXECUTE FUNCTION qp_oc_fn();
         CREATE TRIGGER qp_oc_stmt AFTER UPDATE ON qp_oc_t
             FOR EACH STATEMENT EXECUTE FUNCTION qp_oc_fn();
         GRANT SELECT, INSERT, UPDATE ON qp_oc_t TO qp_oc_app;",
    )]);
    // DO NOTHING doesn't update a row.
    allowed(
        &db,
        "qp_oc_app",
        "INSERT INTO qp_oc_t (id, a) VALUES (1, 2) ON CONFLICT (id) DO NOTHING",
    );
    // DO UPDATE fires the row-level UPDATE trigger (not the statement-level
    // one, whose body would be checked the same way but doesn't run).
    denied_with(
        &db,
        "qp_oc_app",
        "INSERT INTO qp_oc_t (id, a) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a",
        "permission denied for table qp_oc_audit",
    );
}

#[test]
fn on_conflict_do_update_fires_statement_update_triggers() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_ocs_app;
         CREATE TABLE qp_ocs_t (id int PRIMARY KEY, a int);
         INSERT INTO qp_ocs_t VALUES (1, 1);
         CREATE TABLE qp_ocs_audit (id int);
         CREATE FUNCTION qp_ocs_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_ocs_audit (id) VALUES (1);
           RETURN NULL;
         END $$;
         CREATE TRIGGER qp_ocs_trg AFTER UPDATE ON qp_ocs_t
             FOR EACH STATEMENT EXECUTE FUNCTION qp_ocs_fn();
         GRANT SELECT, INSERT, UPDATE ON qp_ocs_t TO qp_ocs_app;",
    )]);
    denied_with(
        &db,
        "qp_ocs_app",
        "INSERT INTO qp_ocs_t (id, a) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a",
        "permission denied for table qp_ocs_audit",
    );
}

#[test]
fn merge_actions_fire_their_triggers() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_mrgact_app;
         CREATE TABLE qp_merge_t (id int PRIMARY KEY, a int);
         CREATE TABLE qp_merge_src (id int, a int);
         INSERT INTO qp_merge_t VALUES (1, 1);
         INSERT INTO qp_merge_src VALUES (1, 2), (2, 2);
         CREATE TABLE qp_merge_audit (id int);
         CREATE FUNCTION qp_merge_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_merge_audit (id) VALUES (1);
           RETURN NULL;
         END $$;
         CREATE TRIGGER qp_merge_upd AFTER UPDATE ON qp_merge_t
             FOR EACH ROW EXECUTE FUNCTION qp_merge_fn();
         CREATE TRIGGER qp_merge_ins AFTER INSERT ON qp_merge_t
             FOR EACH ROW EXECUTE FUNCTION qp_merge_fn();
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_merge_t TO qp_mrgact_app;
         GRANT SELECT ON qp_merge_src TO qp_mrgact_app;",
    )]);
    denied_with(
        &db,
        "qp_mrgact_app",
        "MERGE INTO qp_merge_t t USING qp_merge_src s ON t.id = s.id
         WHEN MATCHED THEN UPDATE SET a = s.a
         WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
        "permission denied for table qp_merge_audit",
    );
}

#[test]
fn instead_of_trigger_body_close_reads_write_tables() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_itb_app;
         CREATE TABLE qp_itb_base (id int, a int);
         CREATE TABLE qp_itb_audit (id int);
         CREATE VIEW qp_itb_v AS SELECT id, a FROM qp_itb_base;
         CREATE FUNCTION qp_itb_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_itb_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_itb_trg INSTEAD OF INSERT ON qp_itb_v
             FOR EACH ROW EXECUTE FUNCTION qp_itb_fn();
         GRANT SELECT, INSERT ON qp_itb_v TO qp_itb_app;",
    )]);
    let e = denied_with(
        &db,
        "qp_itb_app",
        "INSERT INTO qp_itb_v (id, a) VALUES (1, 1)",
        "permission denied for table qp_itb_audit",
    );
    assert!(
        e.contains("in trigger function public.qp_itb_fn(), fired by INSERT on public.qp_itb_v"),
        "{e}"
    );
}

#[test]
fn auto_updatable_view_base_table_trigger_runs_as_the_caller() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_av_app;
         CREATE TABLE qp_av_base (id int, a int);
         CREATE TABLE qp_av_audit (id int);
         CREATE VIEW qp_av_v AS SELECT id, a FROM qp_av_base;
         CREATE FUNCTION qp_av_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_av_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_av_trg AFTER INSERT ON qp_av_base
             FOR EACH ROW EXECUTE FUNCTION qp_av_fn();
         GRANT SELECT, INSERT ON qp_av_v TO qp_av_app;",
    )]);
    let e = denied_with(
        &db,
        "qp_av_app",
        "INSERT INTO qp_av_v (id, a) VALUES (1, 1)",
        "permission denied for table qp_av_audit",
    );
    assert!(
        e.contains(
            "in trigger function public.qp_av_fn(), fired by INSERT on public.qp_av_base, \
             run as role qp_av_app"
        ),
        "{e}"
    );
}

#[test]
fn trigger_body_resolves_new_old_and_local_variables() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vars_app;
         CREATE TABLE qp_vars_t (id int, a int);
         INSERT INTO qp_vars_t VALUES (1, 1);
         CREATE TABLE qp_vars_audit (id int, v int, old_id int);
         CREATE FUNCTION qp_vars_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE
           v int := 1;
         BEGIN
           v := NEW.id + v;
           INSERT INTO qp_vars_audit (id, v, old_id)
             VALUES (NEW.id, v, OLD.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_vars_trg BEFORE UPDATE ON qp_vars_t
             FOR EACH ROW EXECUTE FUNCTION qp_vars_fn();
         GRANT SELECT, UPDATE ON qp_vars_t TO qp_vars_app;",
    )]);
    denied_with(
        &db,
        "qp_vars_app",
        "UPDATE qp_vars_t SET a = 2",
        "permission denied for table qp_vars_audit",
    );
}

#[test]
fn trigger_calls_function_reading_protected_table() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_rec_app;
         CREATE TABLE qp_rec_t (id int, a int);
         CREATE TABLE qp_rec_secret (id int);
         INSERT INTO qp_rec_secret VALUES (1);
         CREATE FUNCTION qp_rec_read() RETURNS int LANGUAGE sql AS
             'SELECT max(id) FROM qp_rec_secret';
         CREATE FUNCTION qp_rec_trg_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           PERFORM qp_rec_read();
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_rec_trg AFTER INSERT ON qp_rec_t
             FOR EACH ROW EXECUTE FUNCTION qp_rec_trg_fn();
         GRANT SELECT, INSERT ON qp_rec_t TO qp_rec_app;",
    )]);
    let e = denied_with(
        &db,
        "qp_rec_app",
        "INSERT INTO qp_rec_t (id, a) VALUES (1, 1)",
        "permission denied for table qp_rec_secret",
    );
    assert!(
        e.contains(
            "in function public.qp_rec_read(), called by trigger function public.qp_rec_trg_fn(), \
             fired by INSERT on public.qp_rec_t, run as role qp_rec_app"
        ),
        "{e}"
    );
}

#[test]
fn trigger_body_writes_table_with_its_own_trigger() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_nest_app;
         CREATE TABLE qp_nest_t (id int, a int);
         CREATE TABLE qp_nest_mid (id int);
         CREATE TABLE qp_nest_audit (id int);
         CREATE FUNCTION qp_nest_fn_a() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_nest_mid (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE FUNCTION qp_nest_fn_b() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_nest_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_nest_trg_a AFTER INSERT ON qp_nest_t
             FOR EACH ROW EXECUTE FUNCTION qp_nest_fn_a();
         CREATE TRIGGER qp_nest_trg_b AFTER INSERT ON qp_nest_mid
             FOR EACH ROW EXECUTE FUNCTION qp_nest_fn_b();
         GRANT SELECT, INSERT ON qp_nest_t, qp_nest_mid TO qp_nest_app;",
    )]);
    let e = denied_with(
        &db,
        "qp_nest_app",
        "INSERT INTO qp_nest_t (id, a) VALUES (1, 1)",
        "permission denied for table qp_nest_audit",
    );
    assert!(
        e.contains(
            "in trigger function public.qp_nest_fn_b(), fired by INSERT on public.qp_nest_mid, \
             called by trigger function public.qp_nest_fn_a()"
        ),
        "{e}"
    );
}

#[test]
fn trigger_cycle_terminates() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_cycle_app;
         CREATE TABLE qp_cycle_a (id int);
         CREATE TABLE qp_cycle_b (id int);
         CREATE FUNCTION qp_cycle_fn_a() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF pg_trigger_depth() > 3 THEN RETURN NEW; END IF;
           INSERT INTO qp_cycle_b (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE FUNCTION qp_cycle_fn_b() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF pg_trigger_depth() > 3 THEN RETURN NEW; END IF;
           INSERT INTO qp_cycle_a (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_cycle_trg_a AFTER INSERT ON qp_cycle_a
             FOR EACH ROW EXECUTE FUNCTION qp_cycle_fn_a();
         CREATE TRIGGER qp_cycle_trg_b AFTER INSERT ON qp_cycle_b
             FOR EACH ROW EXECUTE FUNCTION qp_cycle_fn_b();
         GRANT SELECT, INSERT ON qp_cycle_a, qp_cycle_b TO qp_cycle_app;",
    )]);
    allowed(
        &db,
        "qp_cycle_app",
        "INSERT INTO qp_cycle_a (id) VALUES (1)",
    );
}

#[test]
fn disabled_triggers_do_not_run() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dis_app;
         CREATE TABLE qp_dis_t (id int, a int);
         CREATE TABLE qp_dis_audit (id int);
         CREATE FUNCTION qp_dis_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_dis_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_dis_trg AFTER INSERT ON qp_dis_t
             FOR EACH ROW EXECUTE FUNCTION qp_dis_fn();
         ALTER TABLE qp_dis_t DISABLE TRIGGER qp_dis_trg;
         GRANT SELECT, INSERT ON qp_dis_t TO qp_dis_app;",
    )]);
    allowed(
        &db,
        "qp_dis_app",
        "INSERT INTO qp_dis_t (id, a) VALUES (1, 1)",
    );

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dis_all_app;
         CREATE TABLE qp_dis_all_t (id int, a int);
         CREATE TABLE qp_dis_all_audit (id int);
         CREATE FUNCTION qp_dis_all_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_dis_all_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_dis_all_trg AFTER INSERT ON qp_dis_all_t
             FOR EACH ROW EXECUTE FUNCTION qp_dis_all_fn();
         ALTER TABLE qp_dis_all_t DISABLE TRIGGER ALL;
         GRANT SELECT, INSERT ON qp_dis_all_t TO qp_dis_all_app;",
    )]);
    allowed(
        &db,
        "qp_dis_all_app",
        "INSERT INTO qp_dis_all_t (id, a) VALUES (1, 1)",
    );

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_ena_app;
         CREATE TABLE qp_ena_t (id int, a int);
         CREATE TABLE qp_ena_audit (id int);
         CREATE FUNCTION qp_ena_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_ena_audit (id) VALUES (NEW.id);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_ena_trg AFTER INSERT ON qp_ena_t
             FOR EACH ROW EXECUTE FUNCTION qp_ena_fn();
         ALTER TABLE qp_ena_t DISABLE TRIGGER qp_ena_trg;
         ALTER TABLE qp_ena_t ENABLE TRIGGER qp_ena_trg;
         GRANT SELECT, INSERT ON qp_ena_t TO qp_ena_app;",
    )]);
    denied_with(
        &db,
        "qp_ena_app",
        "INSERT INTO qp_ena_t (id, a) VALUES (1, 1)",
        "permission denied for table qp_ena_audit",
    );
}

#[test]
fn plpgsql_function_body_reads_protected_table() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_plpgsql_app;
         CREATE TABLE qp_plpgsql_secret (id int);
         INSERT INTO qp_plpgsql_secret VALUES (1);
         CREATE FUNCTION qp_plpgsql_f(x int) RETURNS int LANGUAGE plpgsql AS $$
         BEGIN
           RETURN (SELECT count(*) FROM qp_plpgsql_secret WHERE id = x);
         END $$;
         GRANT EXECUTE ON FUNCTION qp_plpgsql_f(int) TO qp_plpgsql_app;",
    )]);
    let e = denied_with(
        &db,
        "qp_plpgsql_app",
        "SELECT qp_plpgsql_f(1)",
        "permission denied for table qp_plpgsql_secret",
    );
    assert!(
        e.contains("in function public.qp_plpgsql_f(integer), called by the statement, run as role qp_plpgsql_app"),
        "{e}"
    );
}

#[test]
fn sql_function_body_reads_protected_table() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_sqlf_app;
         CREATE TABLE qp_sqlf_secret (id int);
         INSERT INTO qp_sqlf_secret VALUES (1);
         CREATE FUNCTION qp_sqlf_f() RETURNS SETOF int LANGUAGE sql
             SECURITY INVOKER AS 'SELECT id FROM qp_sqlf_secret';
         GRANT EXECUTE ON FUNCTION qp_sqlf_f() TO qp_sqlf_app;",
    )]);
    denied_with(
        &db,
        "qp_sqlf_app",
        "SELECT qp_sqlf_f()",
        "permission denied for table qp_sqlf_secret",
    );
}

#[test]
fn inlined_sql_function_body_is_checked() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_inline_app;
         CREATE TABLE qp_inline_secret (id int);
         INSERT INTO qp_inline_secret VALUES (1);
         CREATE FUNCTION qp_inline_f() RETURNS int LANGUAGE sql IMMUTABLE AS
             'SELECT (SELECT max(id) FROM qp_inline_secret)';
         GRANT EXECUTE ON FUNCTION qp_inline_f() TO qp_inline_app;",
    )]);
    denied_with(
        &db,
        "qp_inline_app",
        "SELECT qp_inline_f()",
        "permission denied for table qp_inline_secret",
    );
}

#[test]
fn function_set_search_path_decides_its_body_names() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_sp_app;
         CREATE SCHEMA qp_sp_s;
         CREATE TABLE qp_sp_s.qp_sp_secret (id int);
         INSERT INTO qp_sp_s.qp_sp_secret VALUES (1);
         CREATE FUNCTION qp_sp_f() RETURNS int LANGUAGE sql
             SET search_path = qp_sp_s AS 'SELECT max(id) FROM qp_sp_secret';
         GRANT USAGE ON SCHEMA qp_sp_s TO qp_sp_app;
         GRANT EXECUTE ON FUNCTION qp_sp_f() TO qp_sp_app;",
    )]);
    denied_with(
        &db,
        "qp_sp_app",
        "SELECT qp_sp_f()",
        "permission denied for table qp_sp_secret",
    );

    // Granted, the name resolves and the body may read it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_sp_ok_app;
         CREATE SCHEMA qp_sp_ok_s;
         CREATE TABLE qp_sp_ok_s.qp_sp_ok_secret (id int);
         INSERT INTO qp_sp_ok_s.qp_sp_ok_secret VALUES (1);
         CREATE FUNCTION qp_sp_ok_f() RETURNS int LANGUAGE sql
             SET search_path = qp_sp_ok_s AS 'SELECT max(id) FROM qp_sp_ok_secret';
         GRANT USAGE ON SCHEMA qp_sp_ok_s TO qp_sp_ok_app;
         GRANT SELECT ON qp_sp_ok_s.qp_sp_ok_secret TO qp_sp_ok_app;
         GRANT EXECUTE ON FUNCTION qp_sp_ok_f() TO qp_sp_ok_app;",
    )]);
    allowed(&db, "qp_sp_ok_app", "SELECT qp_sp_ok_f()");
}

#[test]
fn dynamic_sql_in_a_body_is_skipped() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dyn_app;
         CREATE TABLE qp_dyn_secret (id int);
         INSERT INTO qp_dyn_secret VALUES (1);
         CREATE FUNCTION qp_dyn_f() RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
           IF false THEN
             EXECUTE 'SELECT * FROM qp_dyn_secret';
           END IF;
         END $$;
         GRANT EXECUTE ON FUNCTION qp_dyn_f() TO qp_dyn_app;",
    )]);
    // The dynamic statement would read a protected table; it is skipped,
    // and never runs.
    allowed(&db, "qp_dyn_app", "SELECT qp_dyn_f()");
}

#[test]
fn procedure_body_writes_protected_table() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_proc_app;
         CREATE TABLE qp_proc_audit (id int);
         CREATE PROCEDURE qp_proc_p() LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_proc_audit (id) VALUES (1);
         END $$;
         GRANT EXECUTE ON PROCEDURE qp_proc_p() TO qp_proc_app;",
    )]);
    let e = denied_with(
        &db,
        "qp_proc_app",
        "CALL qp_proc_p()",
        "permission denied for table qp_proc_audit",
    );
    assert!(
        e.contains(
            "in procedure public.qp_proc_p(), called by the statement, run as role qp_proc_app"
        ),
        "{e}"
    );
}

#[test]
fn rendered_error_of_a_trigger_body_names_the_trigger() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_render_app;
         CREATE TABLE qp_render_t (id int, a int);
         CREATE TABLE qp_render_audit (id int, a int);
         CREATE FUNCTION qp_render_fn() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_render_audit (id, a) VALUES (NEW.id, NEW.a);
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_render_trg AFTER INSERT ON qp_render_t
             FOR EACH ROW EXECUTE FUNCTION qp_render_fn();
         GRANT SELECT, INSERT ON qp_render_t TO qp_render_app;",
    )]);
    let e = denied(
        &db,
        "qp_render_app",
        "INSERT INTO qp_render_t (id, a) VALUES (1, 1)",
    );
    assert_eq!(
        e,
        "permission denied for table qp_render_audit\n  \
         help: grant it in a migration: GRANT INSERT (id, a) ON public.qp_render_audit TO qp_render_app;\n  \
         note: in trigger function public.qp_render_fn(), fired by INSERT on public.qp_render_t, run as role qp_render_app\n  \
         note: role qp_render_app has no INSERT on column(s) id, a of public.qp_render_audit\n"
    );
}

#[test]
fn row_triggers_of_partitions_and_inheritance_children_fire() {
    // A trigger made directly on a partition fires for the rows routed to
    // it, even when the statement writes the partitioned parent.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_parttrg_app;
         CREATE TABLE audit (note text);
         CREATE TABLE p (k int, v text) PARTITION BY LIST (k);
         CREATE TABLE p1 PARTITION OF p FOR VALUES IN (1);
         CREATE FUNCTION p1_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             INSERT INTO audit (note) VALUES (NEW.v);
             RETURN NEW;
         END $$;
         CREATE TRIGGER p1_audit BEFORE INSERT ON p1 FOR EACH ROW EXECUTE FUNCTION p1_audit();
         GRANT INSERT ON p TO qp_parttrg_app;",
    )]);
    let e = denied(
        &db,
        "qp_parttrg_app",
        "INSERT INTO p (k, v) VALUES (1, 'x')",
    );
    assert!(e.starts_with("permission denied for table audit"), "{e}");

    // An UPDATE of an inheritance parent reaches its children's rows, and
    // their row triggers.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_inhtrg_app;
         CREATE TABLE audit (note text);
         CREATE TABLE parent (id int, v text);
         CREATE TABLE child () INHERITS (parent);
         INSERT INTO child VALUES (1, 'a');
         CREATE FUNCTION child_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             INSERT INTO audit (note) VALUES (OLD.v);
             RETURN NEW;
         END $$;
         CREATE TRIGGER child_audit BEFORE UPDATE ON child FOR EACH ROW EXECUTE FUNCTION child_audit();
         GRANT SELECT, UPDATE ON parent TO qp_inhtrg_app;",
    )]);
    let e = denied(
        &db,
        "qp_inhtrg_app",
        "UPDATE parent SET v = 'b' WHERE id = 1",
    );
    assert!(e.starts_with("permission denied for table audit"), "{e}");
    // An INSERT into the parent stays in the parent.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_inhtrg_ins_app;
         CREATE TABLE audit (note text);
         CREATE TABLE parent (id int, v text);
         CREATE TABLE child () INHERITS (parent);
         CREATE FUNCTION child_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             INSERT INTO audit (note) VALUES (NEW.v);
             RETURN NEW;
         END $$;
         CREATE TRIGGER child_audit BEFORE INSERT ON child FOR EACH ROW EXECUTE FUNCTION child_audit();
         GRANT INSERT ON parent TO qp_inhtrg_ins_app;",
    )]);
    allowed(
        &db,
        "qp_inhtrg_ins_app",
        "INSERT INTO parent (id, v) VALUES (1, 'x')",
    );
}

// ─── Privileges the rewriter and the executor add ──────────────────────────

#[test]
fn view_insert_fills_the_base_tables_defaults() {
    // rewriteTargetView hands the INSERT to the base relation; a column the
    // view leaves out keeps the base's own default (serial's nextval, a
    // function), evaluated as the caller.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vdef_app;
         CREATE ROLE qp_vdef_seq;
         CREATE ROLE qp_vdef_ok;
         CREATE FUNCTION qp_vdef_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         CREATE TABLE qp_vdef_t (id serial, n int DEFAULT qp_vdef_f(), name text);
         CREATE VIEW qp_vdef_v AS SELECT id, n, name FROM qp_vdef_t;
         GRANT INSERT ON qp_vdef_v TO qp_vdef_app, qp_vdef_seq, qp_vdef_ok;
         GRANT USAGE ON SEQUENCE qp_vdef_t_id_seq TO qp_vdef_seq, qp_vdef_ok;
         REVOKE EXECUTE ON FUNCTION qp_vdef_f() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_vdef_f() TO qp_vdef_app, qp_vdef_ok;",
    )]);
    for sql in [
        "INSERT INTO qp_vdef_v (name) VALUES ('x')",
        "INSERT INTO qp_vdef_v DEFAULT VALUES",
    ] {
        denied_with(
            &db,
            "qp_vdef_app",
            sql,
            "permission denied for sequence qp_vdef_t_id_seq",
        );
    }
    // The sequence is granted, the default function is not.
    denied_with(
        &db,
        "qp_vdef_seq",
        "INSERT INTO qp_vdef_v (name) VALUES ('x')",
        "permission denied for function qp_vdef_f",
    );
    allowed(
        &db,
        "qp_vdef_ok",
        "INSERT INTO qp_vdef_v (name) VALUES ('x')",
    );
    // Writing every defaulted column skips both defaults.
    allowed(
        &db,
        "qp_vdef_app",
        "INSERT INTO qp_vdef_v (id, n) VALUES (1, 2)",
    );

    // A default on the view column wins over the base's for that column:
    // the serial is never called.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vdef2_app;
         CREATE ROLE qp_vdef2_ok;
         CREATE FUNCTION qp_vdef2_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         CREATE TABLE qp_vdef2_t (id serial, n int DEFAULT qp_vdef2_f(), name text);
         CREATE VIEW qp_vdef2_v AS SELECT id, n, name FROM qp_vdef2_t;
         ALTER VIEW qp_vdef2_v ALTER COLUMN id SET DEFAULT 42;
         GRANT INSERT ON qp_vdef2_v TO qp_vdef2_app, qp_vdef2_ok;
         REVOKE EXECUTE ON FUNCTION qp_vdef2_f() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_vdef2_f() TO qp_vdef2_ok;",
    )]);
    // `id` takes the view's default, but `n` still runs the base's.
    denied_with(
        &db,
        "qp_vdef2_app",
        "INSERT INTO qp_vdef2_v (name) VALUES ('x')",
        "permission denied for function qp_vdef2_f",
    );
    allowed(
        &db,
        "qp_vdef2_ok",
        "INSERT INTO qp_vdef2_v (name) VALUES ('x')",
    );
}

#[test]
fn view_where_functions_run_as_the_caller() {
    // The view query runs for UPDATE / DELETE (it finds the rows) and for
    // an INSERT into a view with a CHECK OPTION (the WHERE is the WITH
    // CHECK OPTION qual); its functions are checked as the caller.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vwf_app;
         CREATE FUNCTION qp_vwf_f(int) RETURNS bool LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_vwf_f(int) FROM PUBLIC;
         CREATE TABLE qp_vwf_t (a int);
         INSERT INTO qp_vwf_t VALUES (1);
         CREATE VIEW qp_vwf_v AS SELECT a FROM qp_vwf_t WHERE qp_vwf_f(a);
         CREATE VIEW qp_vwf_vc AS SELECT a FROM qp_vwf_t WHERE qp_vwf_f(a)
             WITH CHECK OPTION;
         GRANT ALL ON qp_vwf_v, qp_vwf_vc TO qp_vwf_app;",
    )]);
    for sql in [
        "UPDATE qp_vwf_v SET a = 2",
        "DELETE FROM qp_vwf_v",
        "INSERT INTO qp_vwf_vc VALUES (5)",
    ] {
        denied_with(
            &db,
            "qp_vwf_app",
            sql,
            "permission denied for function qp_vwf_f",
        );
    }
    // Without a CHECK OPTION the INSERT never runs the view's query.
    allowed(&db, "qp_vwf_app", "INSERT INTO qp_vwf_v VALUES (5)");
}

#[test]
fn writing_through_a_view_checks_the_views_columns() {
    // rewriteTargetView leaves the view query's `selectedCols` on the base
    // relation: when the base is read at all, the view owner (or the caller,
    // for a security_invoker view) needs SELECT on every column the view's
    // definition reads — WHERE columns included.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vcol_inv_app;
         CREATE TABLE qp_vcol_inv_t (a int, secret int);
         INSERT INTO qp_vcol_inv_t VALUES (1, 1);
         CREATE VIEW qp_vcol_inv_v WITH (security_invoker) AS
             SELECT a, secret FROM qp_vcol_inv_t;
         GRANT ALL ON qp_vcol_inv_v TO qp_vcol_inv_app;
         GRANT SELECT (a), UPDATE (a) ON qp_vcol_inv_t TO qp_vcol_inv_app;",
    )]);
    // No column of the view is read: the base requires UPDATE only.
    allowed(&db, "qp_vcol_inv_app", "UPDATE qp_vcol_inv_v SET a = 2");
    // The WHERE reads a view column: the base relation is read too, and its
    // selectedCols include `secret`.
    denied_with(
        &db,
        "qp_vcol_inv_app",
        "UPDATE qp_vcol_inv_v SET a = 2 WHERE a = 1",
        "permission denied for table qp_vcol_inv_t",
    );

    // The owner variant: the view's query reads `b` in its WHERE, which the
    // owner may not SELECT.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vcol_owner;
         CREATE ROLE qp_vcol_app;
         CREATE TABLE qp_vcol_t (a int, b int);
         INSERT INTO qp_vcol_t VALUES (1, 1);
         CREATE VIEW qp_vcol_v AS SELECT a FROM qp_vcol_t WHERE b > 0;
         ALTER VIEW qp_vcol_v OWNER TO qp_vcol_owner;
         GRANT UPDATE (a), SELECT (a) ON qp_vcol_t TO qp_vcol_owner;
         GRANT UPDATE (a), SELECT (a) ON qp_vcol_v TO qp_vcol_app;",
    )]);
    allowed(&db, "qp_vcol_app", "UPDATE qp_vcol_v SET a = 2");
    denied_with(
        &db,
        "qp_vcol_app",
        "UPDATE qp_vcol_v SET a = 2 WHERE a = 1",
        "permission denied for table qp_vcol_t",
    );
}

#[test]
fn nested_security_invoker_view_reads_as_the_caller() {
    // RelationBuildRuleLock sets a security_invoker view's checkAsUser to
    // InvalidOid — the statement's role — whatever view encloses it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_inv_outer_owner;
         CREATE ROLE qp_inv_app;
         CREATE TABLE qp_inv_t (id int);
         CREATE VIEW qp_inv_vi WITH (security_invoker = true) AS
             SELECT id FROM qp_inv_t;
         CREATE VIEW qp_inv_vo AS SELECT id FROM qp_inv_vi;
         ALTER VIEW qp_inv_vo OWNER TO qp_inv_outer_owner;
         GRANT SELECT ON qp_inv_vi TO qp_inv_outer_owner;
         GRANT SELECT ON qp_inv_vo TO qp_inv_app;
         GRANT SELECT ON qp_inv_t TO qp_inv_app;",
    )]);
    allowed(&db, "qp_inv_app", "SELECT * FROM qp_inv_vo");

    // Without SELECT for the caller on the base table, the invoker view
    // denies — the enclosing view's owner doesn't stand in for it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_inv2_outer_owner;
         CREATE ROLE qp_inv2_app;
         CREATE TABLE qp_inv2_t (id int);
         CREATE VIEW qp_inv2_vi WITH (security_invoker = true) AS
             SELECT id FROM qp_inv2_t;
         CREATE VIEW qp_inv2_vo AS SELECT id FROM qp_inv2_vi;
         ALTER VIEW qp_inv2_vo OWNER TO qp_inv2_outer_owner;
         GRANT SELECT ON qp_inv2_vi TO qp_inv2_outer_owner;
         GRANT SELECT ON qp_inv2_vo TO qp_inv2_app;
         GRANT SELECT ON qp_inv2_t TO qp_inv2_outer_owner;",
    )]);
    denied_with(
        &db,
        "qp_inv2_app",
        "SELECT * FROM qp_inv2_vo",
        "permission denied for table qp_inv2_t",
    );
}

#[test]
fn constant_regclass_in_a_view_is_not_a_relation_read() {
    // A view naming a relation only as a constant reads nothing from it: the
    // pg_depend edge it records is for DROP, not for SELECT.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_reg_owner;
         CREATE ROLE qp_reg_app;
         CREATE TABLE qp_reg_t (a int);
         CREATE VIEW qp_reg_v AS SELECT 'qp_reg_t'::regclass AS rc;
         ALTER VIEW qp_reg_v OWNER TO qp_reg_owner;
         GRANT SELECT ON qp_reg_v TO qp_reg_app;",
    )]);
    allowed(&db, "qp_reg_app", "SELECT * FROM qp_reg_v");
}

#[test]
fn default_sequence_requirements_follow_the_function() {
    // A stored sequence OID (a bare regclass constant) needs no sequence
    // privilege; only nextval / currval / setval do, with their own masks.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dsq_const;
         CREATE SEQUENCE qp_dsq_s;
         CREATE TABLE qp_dsq_t (ref regclass DEFAULT 'qp_dsq_s'::regclass);
         GRANT INSERT ON qp_dsq_t TO qp_dsq_const;",
    )]);
    allowed(&db, "qp_dsq_const", "INSERT INTO qp_dsq_t DEFAULT VALUES");

    // setval requires UPDATE (USAGE doesn't do).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dsq_set_usage;
         CREATE ROLE qp_dsq_set_update;
         CREATE SEQUENCE qp_dsq_set;
         CREATE TABLE qp_dsq_st (c int DEFAULT setval('qp_dsq_set', 1));
         GRANT INSERT ON qp_dsq_st TO qp_dsq_set_usage, qp_dsq_set_update;
         GRANT USAGE ON SEQUENCE qp_dsq_set TO qp_dsq_set_usage, qp_dsq_set_update;
         GRANT UPDATE ON SEQUENCE qp_dsq_set TO qp_dsq_set_update;",
    )]);
    denied_with(
        &db,
        "qp_dsq_set_usage",
        "INSERT INTO qp_dsq_st DEFAULT VALUES",
        "permission denied for sequence qp_dsq_set",
    );
    allowed(
        &db,
        "qp_dsq_set_update",
        "INSERT INTO qp_dsq_st DEFAULT VALUES",
    );

    // An operator in a default runs its underlying function.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dsq_op;
         CREATE FUNCTION qp_dsq_opfn(int, int) RETURNS int LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN $1 + $2; END $$;
         CREATE OPERATOR public.#=# (leftarg = int, rightarg = int,
             function = qp_dsq_opfn);
         CREATE TABLE qp_dsq_opt (a int DEFAULT (1 #=# 2));
         REVOKE EXECUTE ON FUNCTION qp_dsq_opfn(int, int) FROM PUBLIC;
         GRANT INSERT ON qp_dsq_opt TO qp_dsq_op;",
    )]);
    denied_with(
        &db,
        "qp_dsq_op",
        "INSERT INTO qp_dsq_opt DEFAULT VALUES",
        "permission denied for function qp_dsq_opfn",
    );
}

#[test]
fn sequence_functions_in_views_and_parameter_defaults() {
    // A view's nextval is a sequence check as the caller (a view's functions
    // run as the statement's role).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_sqv_app;
         CREATE ROLE qp_sqv_ok;
         CREATE SEQUENCE qp_sqv_s;
         CREATE VIEW qp_sqv_v AS SELECT nextval('qp_sqv_s') AS n;
         GRANT SELECT ON qp_sqv_v TO qp_sqv_app, qp_sqv_ok;
         GRANT USAGE ON SEQUENCE qp_sqv_s TO qp_sqv_ok;",
    )]);
    denied_with(
        &db,
        "qp_sqv_app",
        "SELECT * FROM qp_sqv_v",
        "permission denied for sequence qp_sqv_s",
    );
    allowed(&db, "qp_sqv_ok", "SELECT * FROM qp_sqv_v");

    // A parameter DEFAULT's nextval runs with the call.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_sqd_app;
         CREATE SEQUENCE qp_sqd_s;
         CREATE FUNCTION qp_sqd_h(x bigint DEFAULT nextval('qp_sqd_s'))
             RETURNS bigint LANGUAGE plpgsql AS $$ BEGIN RETURN x; END $$;
         GRANT EXECUTE ON FUNCTION qp_sqd_h(bigint) TO qp_sqd_app;",
    )]);
    denied_with(
        &db,
        "qp_sqd_app",
        "SELECT qp_sqd_h()",
        "permission denied for sequence qp_sqd_s",
    );
    // Giving the argument skips the default.
    allowed(&db, "qp_sqd_app", "SELECT qp_sqd_h(1)");
}

#[test]
fn only_the_inserted_parameter_defaults_run() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_pdef_app;
         CREATE FUNCTION qp_pdef_secret() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 7; END $$;
         CREATE FUNCTION qp_pdef_f(a int DEFAULT qp_pdef_secret(), b int DEFAULT 0)
             RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$;
         REVOKE EXECUTE ON FUNCTION qp_pdef_secret() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_pdef_f(int, int) TO qp_pdef_app;",
    )]);
    // Only `b`'s default joins the call.
    allowed(&db, "qp_pdef_app", "SELECT qp_pdef_f(1)");
    allowed(&db, "qp_pdef_app", "SELECT qp_pdef_f(1, 2)");
    // `a`'s default is inserted when it is left out, by name or not.
    denied_with(
        &db,
        "qp_pdef_app",
        "SELECT qp_pdef_f()",
        "permission denied for function qp_pdef_secret",
    );
    denied_with(
        &db,
        "qp_pdef_app",
        "SELECT qp_pdef_f(b => 1)",
        "permission denied for function qp_pdef_secret",
    );
}

#[test]
fn check_constraint_functions_run_for_written_rows() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_chk_app;
         CREATE FUNCTION qp_chk_f(int) RETURNS bool LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_chk_f(int) FROM PUBLIC;
         CREATE TABLE qp_chk_t (a int CHECK (qp_chk_f(a)));
         INSERT INTO qp_chk_t VALUES (1);
         GRANT INSERT (a), UPDATE (a) ON qp_chk_t TO qp_chk_app;",
    )]);
    denied_with(
        &db,
        "qp_chk_app",
        "INSERT INTO qp_chk_t VALUES (1)",
        "permission denied for function qp_chk_f",
    );
    denied_with(
        &db,
        "qp_chk_app",
        "UPDATE qp_chk_t SET a = 2",
        "permission denied for function qp_chk_f",
    );
}

#[test]
fn domain_checks_and_defaults_run_for_written_rows() {
    // A column of a domain type runs the domain's DEFAULT when no column
    // default gives it a value, and the domain's CHECKs when a value is
    // stored (nested domains included).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dom_app;
         CREATE FUNCTION qp_dom_g() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_dom_g() FROM PUBLIC;
         CREATE DOMAIN qp_dom_d AS int DEFAULT qp_dom_g();
         CREATE TABLE qp_dom_t (a qp_dom_d, b int);
         GRANT INSERT (b), UPDATE (b) ON qp_dom_t TO qp_dom_app;",
    )]);
    denied_with(
        &db,
        "qp_dom_app",
        "INSERT INTO qp_dom_t (b) VALUES (1)",
        "permission denied for function qp_dom_g",
    );
    // An UPDATE that doesn't touch the domain column runs nothing.
    allowed(&db, "qp_dom_app", "UPDATE qp_dom_t SET b = 2");

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_domchk_app;
         CREATE FUNCTION qp_domchk_f(int) RETURNS bool LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_domchk_f(int) FROM PUBLIC;
         CREATE DOMAIN qp_domchk_d AS int CHECK (qp_domchk_f(VALUE));
         CREATE DOMAIN qp_domchk_d2 AS qp_domchk_d;
         CREATE TABLE qp_domchk_t (a qp_domchk_d2);
         INSERT INTO qp_domchk_t VALUES (1);
         GRANT INSERT (a), UPDATE (a) ON qp_domchk_t TO qp_domchk_app;",
    )]);
    denied_with(
        &db,
        "qp_domchk_app",
        "INSERT INTO qp_domchk_t VALUES (1)",
        "permission denied for function qp_domchk_f",
    );
    denied_with(
        &db,
        "qp_domchk_app",
        "UPDATE qp_domchk_t SET a = 2",
        "permission denied for function qp_domchk_f",
    );
}

#[test]
fn index_expression_functions_run_for_written_rows() {
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_idx_app;
         CREATE FUNCTION qp_idx_fi(int) RETURNS int LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_idx_fi(int) FROM PUBLIC;
         CREATE TABLE qp_idx_t (a int, b int);
         CREATE INDEX qp_idx_expr ON qp_idx_t (qp_idx_fi(a));
         INSERT INTO qp_idx_t VALUES (1, 1);
         GRANT INSERT (a, b), UPDATE (a), UPDATE (b) ON qp_idx_t TO qp_idx_app;",
    )]);
    // Whether an UPDATE goes HOT depends on the data (a row moving needs a
    // second page), so even the index of a column the SET list leaves alone
    // may be evaluated: the policy reports it.
    db.skip_pg_sanity();
    // INSERT indexes every row.
    denied_with(
        &db,
        "qp_idx_app",
        "INSERT INTO qp_idx_t VALUES (1, 1)",
        "permission denied for function qp_idx_fi",
    );
    // Updating a column the index doesn't cover only goes HOT while the
    // row finds room on its page.
    denied_with(
        &db,
        "qp_idx_app",
        "UPDATE qp_idx_t SET b = 2",
        "permission denied for function qp_idx_fi",
    );
    denied_with(
        &db,
        "qp_idx_app",
        "UPDATE qp_idx_t SET a = 3",
        "permission denied for function qp_idx_fi",
    );

    // A partial index's predicate runs for every row the INSERT keeps.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_idxp_app;
         CREATE FUNCTION qp_idxp_fi(int) RETURNS int LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_idxp_fi(int) FROM PUBLIC;
         CREATE TABLE qp_idxp_t (a int, b int);
         CREATE INDEX qp_idxp_expr ON qp_idxp_t (b) WHERE qp_idxp_fi(a) > 0;
         GRANT INSERT (a, b) ON qp_idxp_t TO qp_idxp_app;",
    )]);
    denied_with(
        &db,
        "qp_idxp_app",
        "INSERT INTO qp_idxp_t VALUES (1, 1)",
        "permission denied for function qp_idxp_fi",
    );
}

#[test]
fn generated_columns_run_when_recomputed_or_read() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_gen_app;
         CREATE FUNCTION qp_gen_f(int) RETURNS int LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_gen_f(int) FROM PUBLIC;
         CREATE TABLE qp_gen_t (a int, other int, g int
             GENERATED ALWAYS AS (qp_gen_f(a)) STORED);
         INSERT INTO qp_gen_t (a, other) VALUES (0, 0);
         GRANT UPDATE (a), UPDATE (other) ON qp_gen_t TO qp_gen_app;",
    )]);
    // The UPDATE changes the column the generation expression reads.
    denied_with(
        &db,
        "qp_gen_app",
        "UPDATE qp_gen_t SET a = 1",
        "permission denied for function qp_gen_f",
    );
    // Not touched: the stored column is not recomputed.
    allowed(&db, "qp_gen_app", "UPDATE qp_gen_t SET other = 1");

    // A MERGE UPDATE action recomputes it the same way.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_genm_app;
         CREATE FUNCTION qp_genm_f(int) RETURNS int LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_genm_f(int) FROM PUBLIC;
         CREATE TABLE qp_genm_t (id int, a int, other int, g int
             GENERATED ALWAYS AS (qp_genm_f(a)) STORED);
         INSERT INTO qp_genm_t (id, a, other) VALUES (1, 0, 0);
         GRANT SELECT, UPDATE ON qp_genm_t TO qp_genm_app;",
    )]);
    denied_with(
        &db,
        "qp_genm_app",
        "MERGE INTO qp_genm_t t USING (VALUES (1)) AS s(id) ON t.id = s.id
         WHEN MATCHED THEN UPDATE SET a = 2",
        "permission denied for function qp_genm_f",
    );
    allowed(
        &db,
        "qp_genm_app",
        "MERGE INTO qp_genm_t t USING (VALUES (1)) AS s(id) ON t.id = s.id
         WHEN MATCHED THEN UPDATE SET other = 2",
    );

    // Reading a virtual generated column runs its expression. PG 18 allows
    // only built-in functions there, so the revoked one is `abs`.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_vgen_app;
         CREATE TABLE qp_vgen_t (a int, v int
             GENERATED ALWAYS AS (abs(a)) VIRTUAL);
         INSERT INTO qp_vgen_t (a) VALUES (-1);
         REVOKE EXECUTE ON FUNCTION abs(int) FROM PUBLIC;
         GRANT SELECT (a, v) ON qp_vgen_t TO qp_vgen_app;",
    )]);
    denied_with(
        &db,
        "qp_vgen_app",
        "SELECT v FROM qp_vgen_t",
        "permission denied for function abs",
    );
    allowed(&db, "qp_vgen_app", "SELECT a FROM qp_vgen_t");
}

#[test]
fn merge_update_set_default_runs_the_default() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_mrgdef_app;
         CREATE FUNCTION qp_mrgdef_secret() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_mrgdef_secret() FROM PUBLIC;
         CREATE TABLE qp_mrgdef_t (id int, a int DEFAULT qp_mrgdef_secret());
         INSERT INTO qp_mrgdef_t VALUES (1, 0);
         GRANT SELECT, UPDATE ON qp_mrgdef_t TO qp_mrgdef_app;",
    )]);
    denied_with(
        &db,
        "qp_mrgdef_app",
        "MERGE INTO qp_mrgdef_t t USING (VALUES (1)) AS s(id) ON t.id = s.id
         WHEN MATCHED THEN UPDATE SET a = DEFAULT",
        "permission denied for function qp_mrgdef_secret",
    );
}

#[test]
fn unreferenced_select_cte_is_not_checked() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_cte2_app;
         CREATE TABLE qp_cte2_secret (id int);",
    )]);
    // SS_process_ctes skips a SELECT CTE with cterefcount 0: nothing it
    // reads is checked.
    allowed(
        &db,
        "qp_cte2_app",
        "WITH c AS (SELECT * FROM qp_cte2_secret) SELECT 1",
    );
    allowed(
        &db,
        "qp_cte2_app",
        "WITH c AS MATERIALIZED (SELECT * FROM qp_cte2_secret) SELECT 1",
    );
    // Referenced from the statement (or twice), it is planned.
    denied_with(
        &db,
        "qp_cte2_app",
        "WITH c AS (SELECT * FROM qp_cte2_secret) SELECT * FROM c",
        "permission denied for table qp_cte2_secret",
    );
    denied_with(
        &db,
        "qp_cte2_app",
        "WITH c AS (SELECT * FROM qp_cte2_secret) SELECT 1 FROM c, c c2",
        "permission denied for table qp_cte2_secret",
    );
    // An inlined CTE referenced only by an unplanned one is still not
    // planned...
    allowed(
        &db,
        "qp_cte2_app",
        "WITH c AS (SELECT * FROM qp_cte2_secret), d AS (SELECT * FROM c) SELECT 1",
    );
    // ... but one the planner doesn't inline (MATERIALIZED) is, as soon as
    // anything references it.
    denied_with(
        &db,
        "qp_cte2_app",
        "WITH c AS MATERIALIZED (SELECT * FROM qp_cte2_secret), d AS (SELECT * FROM c) SELECT 1",
        "permission denied for table qp_cte2_secret",
    );
    // A data-modifying CTE always runs.
    denied_with(
        &db,
        "qp_cte2_app",
        "WITH d AS (DELETE FROM qp_cte2_secret RETURNING *) SELECT 1",
        "permission denied for table qp_cte2_secret",
    );
}

#[test]
fn code_the_planner_folds_away_is_not_checked() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_fold_app;
         CREATE FUNCTION qp_fold_g() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_fold_g() FROM PUBLIC;
         CREATE TABLE qp_fold_t (a int);
         GRANT SELECT (a) ON qp_fold_t TO qp_fold_app;
         CREATE TABLE qp_fold_secret (id int);",
    )]);
    // eval_const_expressions drops these before the executor checks them.
    for sql in [
        "SELECT CASE WHEN false THEN qp_fold_g() ELSE 0 END",
        "SELECT CASE WHEN true THEN 0 ELSE qp_fold_g() END",
        "SELECT COALESCE(1, qp_fold_g())",
        "SELECT 1 WHERE false AND EXISTS (SELECT 1 FROM qp_fold_secret)",
        "SELECT 1 WHERE true OR EXISTS (SELECT 1 FROM qp_fold_secret)",
    ] {
        allowed(&db, "qp_fold_app", sql);
    }
    // What survives planning is checked even where no row reaches it, and
    // nothing beyond literal constants is dropped.
    for sql in [
        "SELECT qp_fold_g() FROM qp_fold_t WHERE false",
        "SELECT COALESCE(a, qp_fold_g()) FROM qp_fold_t",
        "SELECT CASE WHEN a IS NULL THEN qp_fold_g() ELSE 0 END FROM qp_fold_t",
    ] {
        denied_with(
            &db,
            "qp_fold_app",
            sql,
            "permission denied for function qp_fold_g",
        );
    }
}

// ─── What a routine or trigger body may run (policy) ──────────────────────
//
// A statement is checked against everything it may run for some data (see
// the module doc of `privileges.rs` and README's "Role checks"): every
// trigger that may fire and every body statement that may be reached.
// Code that provably never runs is pruned; data-dependent branches stay
// checked.

#[test]
fn sql_standard_bodies_are_resolved_at_creation() {
    // PG stores an SQL-standard body analyzed (`prosqlbody`): its objects
    // are the ones the creation search_path found, and running it looks no
    // name up, so no schema USAGE is checked.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_atomic_app;
         CREATE SCHEMA qp_atomic_s;
         CREATE TABLE qp_atomic_s.t (id int);
         GRANT SELECT ON qp_atomic_s.t TO qp_atomic_app;
         CREATE FUNCTION qp_atomic_f() RETURNS bigint LANGUAGE sql
             BEGIN ATOMIC SELECT count(*) FROM qp_atomic_s.t; END;
         CREATE FUNCTION qp_atomic_text() RETURNS bigint LANGUAGE sql
             AS 'SELECT count(*) FROM qp_atomic_s.t';",
    )]);
    allowed(&db, "qp_atomic_app", "SELECT qp_atomic_f()");
    // A string body is analyzed when it first runs, as its caller.
    denied_with(
        &db,
        "qp_atomic_app",
        "SELECT qp_atomic_text()",
        "permission denied for schema qp_atomic_s",
    );

    // The body's names are the ones the creation search_path resolved: the
    // invocation session path doesn't resolve them again.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_atomic_unq_app;
         CREATE SCHEMA qp_atomic_a;
         CREATE TABLE qp_atomic_a.t (id int);
         CREATE TABLE public.qp_atomic_t (id int);
         GRANT USAGE ON SCHEMA qp_atomic_a TO qp_atomic_unq_app;
         GRANT SELECT ON qp_atomic_a.t TO qp_atomic_unq_app;
         SET search_path = qp_atomic_a, public;
         CREATE FUNCTION public.qp_atomic_unq_f() RETURNS bigint LANGUAGE sql
             BEGIN ATOMIC SELECT count(*) FROM t; END;
         RESET search_path;",
    )]);
    // The role's sessions use the default path, where `t` would be
    // public.qp_atomic_t (which it may not read).
    allowed(&db, "qp_atomic_unq_app", "SELECT public.qp_atomic_unq_f()");
}

#[test]
fn security_definer_bodies_use_the_callers_session_search_path() {
    // `SECURITY DEFINER` changes the identity, not the session's
    // search_path: the body reads the caller's path, as the owner.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dpath_app;
         CREATE ROLE qp_dpath_owner;
         CREATE SCHEMA qp_dpath_s;
         CREATE TABLE qp_dpath_s.t (id int);
         CREATE TABLE public.t (id int);
         ALTER ROLE qp_dpath_app SET search_path = qp_dpath_s;
         GRANT USAGE ON SCHEMA qp_dpath_s TO qp_dpath_app, qp_dpath_owner;
         GRANT SELECT ON qp_dpath_s.t TO qp_dpath_owner;
         CREATE FUNCTION qp_dpath_f() RETURNS bigint LANGUAGE sql
             SECURITY DEFINER AS 'SELECT count(*) FROM t';
         ALTER FUNCTION qp_dpath_f() OWNER TO qp_dpath_owner;",
    )]);
    // The owner has no setting of its own: its default path would name
    // public.t, which it may not read. The caller's path names
    // qp_dpath_s.t.
    allowed(&db, "qp_dpath_app", "SELECT public.qp_dpath_f()");
}

#[test]
fn called_bodies_inherit_the_enclosing_set_search_path() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_npath_app;
         CREATE SCHEMA qp_npath_s;
         CREATE TABLE qp_npath_s.t (id int);
         CREATE TABLE public.t (id int);
         GRANT USAGE ON SCHEMA qp_npath_s TO qp_npath_app;
         GRANT SELECT ON qp_npath_s.t TO qp_npath_app;
         CREATE FUNCTION qp_npath_inner() RETURNS bigint LANGUAGE plpgsql AS $$
         BEGIN RETURN (SELECT count(*) FROM t); END $$;
         CREATE FUNCTION qp_npath_outer() RETURNS bigint LANGUAGE plpgsql
             SET search_path = qp_npath_s, public AS $$
         BEGIN RETURN public.qp_npath_inner(); END $$;
         GRANT EXECUTE ON FUNCTION qp_npath_outer(), qp_npath_inner()
             TO qp_npath_app;",
    )]);
    // The inner body has no SET: it inherits the outer body's path and
    // reads qp_npath_s.t, not the caller's public.t.
    allowed(&db, "qp_npath_app", "SELECT qp_npath_outer()");
}

#[test]
fn only_writes_ignore_inheritance_children() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_only_app;
         CREATE TABLE qp_only_p (id int, x int);
         CREATE TABLE qp_only_c () INHERITS (qp_only_p);
         CREATE TABLE qp_only_audit (v int);
         INSERT INTO qp_only_p VALUES (1, 1);
         INSERT INTO qp_only_c VALUES (2, 2);
         CREATE FUNCTION qp_only_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_only_audit VALUES (OLD.id); RETURN NEW; END $$;
         CREATE TRIGGER qp_only_c_trg AFTER UPDATE OR DELETE ON qp_only_c
             FOR EACH ROW EXECUTE FUNCTION qp_only_trg();
         GRANT SELECT, UPDATE, DELETE ON qp_only_p TO qp_only_app;",
    )]);
    // ONLY reaches no child rows, so the child's row triggers never fire.
    allowed(&db, "qp_only_app", "UPDATE ONLY qp_only_p SET x = 1");
    allowed(&db, "qp_only_app", "DELETE FROM ONLY qp_only_p");
    denied_with(
        &db,
        "qp_only_app",
        "UPDATE qp_only_p SET x = 1",
        "permission denied for table qp_only_audit",
    );
    denied_with(
        &db,
        "qp_only_app",
        "DELETE FROM qp_only_p",
        "permission denied for table qp_only_audit",
    );

    // `ONLY` on a partitioned table is allowed (it reaches no partition
    // rows, so no row trigger fires).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_onlypart_app;
         CREATE TABLE qp_onlypart_p (k int, x int) PARTITION BY LIST (k);
         CREATE TABLE qp_onlypart_p1 PARTITION OF qp_onlypart_p FOR VALUES IN (1);
         CREATE TABLE qp_onlypart_audit (v int);
         INSERT INTO qp_onlypart_p VALUES (1, 1);
         CREATE FUNCTION qp_onlypart_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_onlypart_audit VALUES (OLD.k); RETURN NEW; END $$;
         CREATE TRIGGER qp_onlypart_trg AFTER UPDATE OR DELETE ON qp_onlypart_p1
             FOR EACH ROW EXECUTE FUNCTION qp_onlypart_trg();
         GRANT SELECT, UPDATE, DELETE ON qp_onlypart_p TO qp_onlypart_app;",
    )]);
    allowed(
        &db,
        "qp_onlypart_app",
        "UPDATE ONLY qp_onlypart_p SET x = 1",
    );
    allowed(&db, "qp_onlypart_app", "DELETE FROM ONLY qp_onlypart_p");
    denied_with(
        &db,
        "qp_onlypart_app",
        "UPDATE qp_onlypart_p SET x = 1",
        "permission denied for table qp_onlypart_audit",
    );
    denied_with(
        &db,
        "qp_onlypart_app",
        "DELETE FROM qp_onlypart_p",
        "permission denied for table qp_onlypart_audit",
    );
}

#[test]
fn partitioned_row_triggers_fire_with_their_clones_state() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_states_app;
         CREATE TABLE qp_states_audit (v int);
         CREATE FUNCTION qp_states_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_states_audit VALUES (NEW.id); RETURN NEW; END $$;
         CREATE TABLE qp_states_rep (id int);
         CREATE TRIGGER x AFTER INSERT ON qp_states_rep
             FOR EACH ROW EXECUTE FUNCTION qp_states_trg();
         ALTER TABLE qp_states_rep ENABLE REPLICA TRIGGER x;
         CREATE TABLE qp_states_alw (id int);
         CREATE TRIGGER x AFTER INSERT ON qp_states_alw
             FOR EACH ROW EXECUTE FUNCTION qp_states_trg();
         ALTER TABLE qp_states_alw ENABLE ALWAYS TRIGGER x;
         CREATE TABLE qp_states_dis (id int);
         CREATE TRIGGER x AFTER INSERT ON qp_states_dis
             FOR EACH ROW EXECUTE FUNCTION qp_states_trg();
         ALTER TABLE qp_states_dis DISABLE TRIGGER x;
         CREATE TABLE qp_states_pt (id int, k int) PARTITION BY LIST (k);
         CREATE TABLE qp_states_pt1 PARTITION OF qp_states_pt FOR VALUES IN (1);
         CREATE TRIGGER x AFTER INSERT ON qp_states_pt
             FOR EACH ROW EXECUTE FUNCTION qp_states_trg();
         ALTER TABLE qp_states_pt1 DISABLE TRIGGER x;
         CREATE TABLE qp_states_ptb (id int, k int) PARTITION BY LIST (k);
         CREATE TRIGGER x AFTER INSERT ON qp_states_ptb
             FOR EACH ROW EXECUTE FUNCTION qp_states_trg();
         ALTER TABLE qp_states_ptb DISABLE TRIGGER x;
         CREATE TABLE qp_states_ptb1 PARTITION OF qp_states_ptb FOR VALUES IN (1);
         CREATE TABLE qp_states_pte (id int, k int) PARTITION BY LIST (k);
         CREATE TABLE qp_states_pte1 PARTITION OF qp_states_pte FOR VALUES IN (1);
         CREATE TRIGGER x AFTER INSERT ON qp_states_pte
             FOR EACH ROW EXECUTE FUNCTION qp_states_trg();
         CREATE TABLE qp_states_stm (id int, k int) PARTITION BY LIST (k);
         CREATE TABLE qp_states_stm1 PARTITION OF qp_states_stm FOR VALUES IN (1);
         CREATE FUNCTION qp_states_stm_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_states_audit VALUES (0); RETURN NULL; END $$;
         CREATE TRIGGER stm AFTER INSERT ON qp_states_stm
             FOR EACH STATEMENT EXECUTE FUNCTION qp_states_stm_trg();
         GRANT INSERT ON qp_states_rep, qp_states_alw, qp_states_dis,
                          qp_states_pt, qp_states_ptb, qp_states_pte,
                          qp_states_stm TO qp_states_app;",
    )]);
    // REPLICA triggers don't fire in an origin session; ALWAYS ones do.
    allowed(&db, "qp_states_app", "INSERT INTO qp_states_rep VALUES (1)");
    denied_with(
        &db,
        "qp_states_app",
        "INSERT INTO qp_states_alw VALUES (1)",
        "permission denied for table qp_states_audit",
    );
    // A disabled trigger doesn't fire.
    allowed(&db, "qp_states_app", "INSERT INTO qp_states_dis VALUES (1)");
    // A row trigger on a partitioned table never runs there: the partition
    // clone does, with its own state.
    allowed(
        &db,
        "qp_states_app",
        "INSERT INTO qp_states_pt VALUES (1, 1)",
    );
    // The clone a later partition gets copies the parent's disabled state.
    allowed(
        &db,
        "qp_states_app",
        "INSERT INTO qp_states_ptb VALUES (1, 1)",
    );
    denied_with(
        &db,
        "qp_states_app",
        "INSERT INTO qp_states_pte VALUES (1, 1)",
        "permission denied for table qp_states_audit",
    );
    // A statement trigger on the partitioned table itself fires.
    denied_with(
        &db,
        "qp_states_app",
        "INSERT INTO qp_states_stm VALUES (1, 1)",
        "permission denied for table qp_states_audit",
    );
}

#[test]
fn a_shared_trigger_function_is_checked_for_each_firing() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_shared_app;
         CREATE TABLE qp_shared_t1 (id int);
         CREATE TABLE qp_shared_t2 (id int);
         CREATE TABLE qp_shared_audit (id int);
         CREATE TABLE qp_shared_secret (id int);
         GRANT SELECT, INSERT ON qp_shared_t1, qp_shared_t2
             TO qp_shared_app;
         GRANT INSERT ON qp_shared_audit TO qp_shared_app;
         CREATE FUNCTION qp_shared_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_shared_audit VALUES (NEW.id);
           IF TG_TABLE_NAME = 'qp_shared_t2' THEN
             INSERT INTO qp_shared_secret VALUES (NEW.id);
           END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER a AFTER INSERT ON qp_shared_t1
             FOR EACH ROW EXECUTE FUNCTION qp_shared_trg();
         CREATE TRIGGER b AFTER INSERT ON qp_shared_t2
             FOR EACH ROW EXECUTE FUNCTION qp_shared_trg();",
    )]);
    // Both firings run: t1's prunes the secret INSERT (TG_TABLE_NAME is
    // known), t2's doesn't. The one analyzed first must not decide.
    allowed(&db, "qp_shared_app", "INSERT INTO qp_shared_t1 VALUES (1)");
    denied_with(
        &db,
        "qp_shared_app",
        "WITH x AS (INSERT INTO qp_shared_t1 VALUES (1)) \
         INSERT INTO qp_shared_t2 VALUES (2)",
        "permission denied for table qp_shared_secret",
    );
}

#[test]
fn calls_inside_bodies_need_execute_and_are_walked() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_plcall_app;
         CREATE TABLE qp_plcall_t (id int);
         CREATE TABLE qp_plcall_audit (v int);
         GRANT SELECT, INSERT ON qp_plcall_t TO qp_plcall_app;
         CREATE PROCEDURE qp_plcall_p(v int) LANGUAGE sql
             AS 'INSERT INTO qp_plcall_audit VALUES (v)';
         CREATE FUNCTION qp_plcall_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN CALL qp_plcall_p(NEW.id); RETURN NEW; END $$;
         CREATE TRIGGER qp_plcall_trg AFTER INSERT ON qp_plcall_t
             FOR EACH ROW EXECUTE FUNCTION qp_plcall_trg();
         CREATE FUNCTION qp_plcall_sql() RETURNS void LANGUAGE sql
             AS 'CALL qp_plcall_p(1)';
         GRANT EXECUTE ON FUNCTION qp_plcall_sql() TO qp_plcall_app;",
    )]);
    let e = denied_with(
        &db,
        "qp_plcall_app",
        "INSERT INTO qp_plcall_t VALUES (1)",
        "permission denied for table qp_plcall_audit",
    );
    assert!(
        e.contains(
            "in procedure public.qp_plcall_p(integer), called by trigger function \
             public.qp_plcall_trg(), fired by INSERT on public.qp_plcall_t, run as role \
             qp_plcall_app"
        ),
        "{e}"
    );
    // A CALL inside a SQL body is walked too.
    denied_with(
        &db,
        "qp_plcall_app",
        "SELECT qp_plcall_sql()",
        "permission denied for table qp_plcall_audit",
    );

    // EXECUTE on the procedure is checked as the current identity.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_plcall_rev_app;
         CREATE TABLE qp_plcall_rev_t (id int);
         CREATE PROCEDURE qp_plcall_rev_p() LANGUAGE plpgsql AS $$ BEGIN NULL; END $$;
         REVOKE EXECUTE ON PROCEDURE qp_plcall_rev_p() FROM PUBLIC;
         CREATE FUNCTION qp_plcall_rev_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN CALL qp_plcall_rev_p(); RETURN NEW; END $$;
         CREATE TRIGGER qp_plcall_rev_trg AFTER INSERT ON qp_plcall_rev_t
             FOR EACH ROW EXECUTE FUNCTION qp_plcall_rev_trg();
         GRANT INSERT ON qp_plcall_rev_t TO qp_plcall_rev_app;",
    )]);
    denied_with(
        &db,
        "qp_plcall_rev_app",
        "INSERT INTO qp_plcall_rev_t VALUES (1)",
        "permission denied for procedure qp_plcall_rev_p",
    );
}

#[test]
fn plpgsql_records_take_the_shape_of_their_query() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_shape_app;
         CREATE TABLE qp_shape_t (id int, x int);
         CREATE TABLE qp_shape_denied (id int, x int);
         INSERT INTO qp_shape_t VALUES (1, 1);
         GRANT SELECT ON qp_shape_t TO qp_shape_app;
         CREATE FUNCTION qp_shape_into() RETURNS SETOF int LANGUAGE plpgsql AS $$
         DECLARE r record;
         BEGIN
           SELECT id, x INTO r FROM qp_shape_t;
           PERFORM 1 FROM qp_shape_denied WHERE id = r.id;
         END $$;
         CREATE FUNCTION qp_shape_for() RETURNS SETOF int LANGUAGE plpgsql AS $$
         DECLARE r record;
         BEGIN
           FOR r IN SELECT id, x FROM qp_shape_t LOOP
             PERFORM 1 FROM qp_shape_denied WHERE id = r.x;
           END LOOP;
         END $$;
         GRANT EXECUTE ON FUNCTION qp_shape_into(), qp_shape_for()
             TO qp_shape_app;",
    )]);
    denied_with(
        &db,
        "qp_shape_app",
        "SELECT * FROM qp_shape_into()",
        "permission denied for table qp_shape_denied",
    );
    denied_with(
        &db,
        "qp_shape_app",
        "SELECT * FROM qp_shape_for()",
        "permission denied for table qp_shape_denied",
    );
}

#[test]
fn execute_using_expressions_are_checked() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_using_app;
         CREATE TABLE qp_using_denied (id int);
         CREATE FUNCTION qp_using_f() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE n int;
         BEGIN
           EXECUTE 'SELECT 1' INTO n
             USING (SELECT count(*) FROM qp_using_denied);
         END $$;
         GRANT EXECUTE ON FUNCTION qp_using_f() TO qp_using_app;",
    )]);
    // The dynamic string stays unchecked; the USING expression runs.
    denied_with(
        &db,
        "qp_using_app",
        "SELECT qp_using_f()",
        "permission denied for table qp_using_denied",
    );
}

#[test]
fn transition_tables_are_in_scope_without_privileges() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_trans_app;
         CREATE TABLE qp_trans_t (id int);
         CREATE TABLE qp_trans_audit (v int);
         INSERT INTO qp_trans_t VALUES (1);
         CREATE FUNCTION qp_trans_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_trans_audit SELECT id FROM new_rows; RETURN NULL; END $$;
         CREATE TRIGGER qp_trans_trg AFTER INSERT ON qp_trans_t
             REFERENCING NEW TABLE AS new_rows
             FOR EACH STATEMENT EXECUTE FUNCTION qp_trans_trg();
         GRANT SELECT, INSERT ON qp_trans_t TO qp_trans_app;",
    )]);
    // `new_rows` has the trigger relation's columns: the statement analyzes
    // and its INSERT into the audit table is checked.
    denied_with(
        &db,
        "qp_trans_app",
        "INSERT INTO qp_trans_t VALUES (2)",
        "permission denied for table qp_trans_audit",
    );

    // Reading the transition rows needs no privilege on the trigger
    // relation: INSERT on the table and the audit table suffice.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_trans_ok_app;
         CREATE TABLE qp_trans_ok_t (id int);
         CREATE TABLE qp_trans_ok_audit (v int);
         CREATE FUNCTION qp_trans_ok_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_trans_ok_audit SELECT id FROM new_rows; RETURN NULL; END $$;
         CREATE TRIGGER qp_trans_ok_trg AFTER INSERT ON qp_trans_ok_t
             REFERENCING NEW TABLE AS new_rows
             FOR EACH STATEMENT EXECUTE FUNCTION qp_trans_ok_trg();
         GRANT INSERT ON qp_trans_ok_t, qp_trans_ok_audit TO qp_trans_ok_app;",
    )]);
    allowed(
        &db,
        "qp_trans_ok_app",
        "INSERT INTO qp_trans_ok_t VALUES (1)",
    );
}

#[test]
fn update_of_a_generated_column_fires_when_its_input_changes() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_genupd_app;
         CREATE TABLE qp_genupd_t (
             id int,
             a int,
             b int,
             g int GENERATED ALWAYS AS (a * 2) STORED);
         CREATE TABLE qp_genupd_audit (v int);
         INSERT INTO qp_genupd_t (id, a, b) VALUES (1, 1, 1);
         CREATE FUNCTION qp_genupd_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_genupd_audit VALUES (NEW.id); RETURN NEW; END $$;
         CREATE TRIGGER qp_genupd_trg AFTER UPDATE OF g ON qp_genupd_t
             FOR EACH ROW EXECUTE FUNCTION qp_genupd_trg();
         GRANT SELECT, UPDATE (a, b) ON qp_genupd_t TO qp_genupd_app;",
    )]);
    // Rewriting `a` recomputes `g`, which counts as updating it.
    denied_with(
        &db,
        "qp_genupd_app",
        "UPDATE qp_genupd_t SET a = 5",
        "permission denied for table qp_genupd_audit",
    );
    // `b` feeds no stored generated column of the trigger list.
    allowed(&db, "qp_genupd_app", "UPDATE qp_genupd_t SET b = 5");
}

#[test]
fn a_moving_update_fires_the_partitions_insert_and_delete_triggers() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_rowmove_app;
         CREATE TABLE qp_rowmove_t (id int, k int) PARTITION BY LIST (k);
         CREATE TABLE qp_rowmove_t1 PARTITION OF qp_rowmove_t FOR VALUES IN (1);
         CREATE TABLE qp_rowmove_t2 PARTITION OF qp_rowmove_t FOR VALUES IN (2);
         CREATE TABLE qp_rowmove_audit (v int);
         INSERT INTO qp_rowmove_t VALUES (1, 1);
         CREATE FUNCTION qp_rowmove_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_rowmove_audit VALUES (NEW.id); RETURN NEW; END $$;
         CREATE TRIGGER qp_rowmove_t2_trg AFTER INSERT ON qp_rowmove_t2
             FOR EACH ROW EXECUTE FUNCTION qp_rowmove_trg();
         GRANT SELECT, UPDATE ON qp_rowmove_t TO qp_rowmove_app;",
    )]);
    denied_with(
        &db,
        "qp_rowmove_app",
        "UPDATE qp_rowmove_t SET k = 2",
        "permission denied for table qp_rowmove_audit",
    );
    // No partition key column changes: the row stays, no INSERT fires.
    allowed(&db, "qp_rowmove_app", "UPDATE qp_rowmove_t SET id = 5");

    // The source partition's DELETE row trigger fires too.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_rowmove_del_app;
         CREATE TABLE qp_rowmove_del_t (id int, k int) PARTITION BY LIST (k);
         CREATE TABLE qp_rowmove_del_t1 PARTITION OF qp_rowmove_del_t FOR VALUES IN (1);
         CREATE TABLE qp_rowmove_del_t2 PARTITION OF qp_rowmove_del_t FOR VALUES IN (2);
         CREATE TABLE qp_rowmove_del_audit (v int);
         INSERT INTO qp_rowmove_del_t VALUES (1, 1);
         CREATE FUNCTION qp_rowmove_del_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_rowmove_del_audit VALUES (OLD.id); RETURN OLD; END $$;
         CREATE TRIGGER qp_rowmove_del_t1_trg AFTER DELETE ON qp_rowmove_del_t1
             FOR EACH ROW EXECUTE FUNCTION qp_rowmove_del_trg();
         GRANT SELECT, UPDATE ON qp_rowmove_del_t TO qp_rowmove_del_app;",
    )]);
    denied_with(
        &db,
        "qp_rowmove_del_app",
        "UPDATE qp_rowmove_del_t SET k = 2",
        "permission denied for table qp_rowmove_del_audit",
    );
}

#[test]
fn trigger_when_conditions_are_checked_and_pruned() {
    // A literal false WHEN never fires: neither its body nor its condition
    // is checked.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_when_app;
         CREATE TABLE qp_when_false_t (id int, x int);
         CREATE TABLE qp_when_audit (v int);
         CREATE FUNCTION qp_when_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_when_audit VALUES (NEW.id); RETURN NEW; END $$;
         CREATE TRIGGER qp_when_false_trg BEFORE INSERT ON qp_when_false_t
             FOR EACH ROW WHEN (false) EXECUTE FUNCTION qp_when_trg();
         GRANT INSERT ON qp_when_false_t TO qp_when_app;",
    )]);
    allowed(
        &db,
        "qp_when_app",
        "INSERT INTO qp_when_false_t VALUES (1, 1)",
    );

    // Otherwise the WHEN condition's functions run as the role running the
    // statement, which needs EXECUTE on them.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_when_fn_app;
         CREATE TABLE qp_when_fn_t (id int, x int);
         CREATE FUNCTION qp_when_fn_check(a int) RETURNS boolean LANGUAGE plpgsql AS $$
         BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_when_fn_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RETURN NEW; END $$;
         REVOKE EXECUTE ON FUNCTION qp_when_fn_check(int) FROM PUBLIC;
         CREATE TRIGGER qp_when_fn_trg BEFORE INSERT ON qp_when_fn_t
             FOR EACH ROW WHEN (qp_when_fn_check(NEW.id))
             EXECUTE FUNCTION qp_when_fn_trg();
         GRANT INSERT ON qp_when_fn_t TO qp_when_fn_app;",
    )]);
    denied_with(
        &db,
        "qp_when_fn_app",
        "INSERT INTO qp_when_fn_t VALUES (1, 1)",
        "permission denied for function qp_when_fn_check",
    );
}

#[test]
fn tg_op_prunes_the_branches_the_firing_cannot_reach() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_tgop_app;
         CREATE TABLE qp_tgop_t (id int PRIMARY KEY, x int);
         CREATE TABLE qp_tgop_ins (v int);
         CREATE TABLE qp_tgop_del (v int);
         INSERT INTO qp_tgop_t VALUES (1, 1);
         GRANT SELECT, INSERT, DELETE ON qp_tgop_t TO qp_tgop_app;
         GRANT INSERT ON qp_tgop_ins TO qp_tgop_app;
         CREATE FUNCTION qp_tgop_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF TG_OP = 'INSERT' THEN
             INSERT INTO qp_tgop_ins VALUES (NEW.id); RETURN NEW;
           ELSE
             INSERT INTO qp_tgop_del VALUES (OLD.id); RETURN OLD;
           END IF;
         END $$;
         CREATE TRIGGER qp_tgop_trg AFTER INSERT OR DELETE ON qp_tgop_t
             FOR EACH ROW EXECUTE FUNCTION qp_tgop_trg();",
    )]);
    allowed(&db, "qp_tgop_app", "INSERT INTO qp_tgop_t VALUES (1, 1)");
    denied_with(
        &db,
        "qp_tgop_app",
        "DELETE FROM qp_tgop_t",
        "permission denied for table qp_tgop_del",
    );

    // TG_WHEN, TG_LEVEL, TG_TABLE_SCHEMA and TG_RELNAME, under AND / OR /
    // NOT and parentheses.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_tgop2_app;
         CREATE TABLE qp_tgop2_t (id int);
         CREATE TABLE qp_tgop2_ok (v int);
         CREATE TABLE qp_tgop2_bad (v int);
         GRANT INSERT ON qp_tgop2_t, qp_tgop2_ok TO qp_tgop2_app;
         CREATE FUNCTION qp_tgop2_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NOT (TG_WHEN = 'BEFORE' AND TG_LEVEL = 'ROW')
              OR TG_TABLE_SCHEMA <> 'public'
              OR TG_RELNAME IN ('qp_tgop2_other', 'qp_tgop2_other2') THEN
             INSERT INTO qp_tgop2_bad VALUES (NEW.id);
           ELSE
             INSERT INTO qp_tgop2_ok VALUES (NEW.id);
           END IF;
           CASE WHEN TG_OP = 'INSERT' THEN
             INSERT INTO qp_tgop2_ok VALUES (NEW.id + 1);
           ELSE
             INSERT INTO qp_tgop2_bad VALUES (NEW.id);
           END CASE;
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_tgop2_trg BEFORE INSERT ON qp_tgop2_t
             FOR EACH ROW EXECUTE FUNCTION qp_tgop2_trg();",
    )]);
    allowed(&db, "qp_tgop2_app", "INSERT INTO qp_tgop2_t VALUES (1)");
}

#[test]
fn exception_handlers_catch_denials() {
    // A handler that catches the denial makes the body's statements safe to
    // fail.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_exc_app;
         CREATE TABLE qp_exc_t (id int);
         CREATE TABLE qp_exc_audit (v int);
         GRANT SELECT, INSERT ON qp_exc_t TO qp_exc_app;
         CREATE FUNCTION qp_exc_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           BEGIN
             INSERT INTO qp_exc_audit VALUES (NEW.id);
           EXCEPTION WHEN insufficient_privilege THEN NULL;
           END;
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_exc_trg AFTER INSERT ON qp_exc_t
             FOR EACH ROW EXECUTE FUNCTION qp_exc_trg();",
    )]);
    allowed(&db, "qp_exc_app", "INSERT INTO qp_exc_t VALUES (1)");

    // So does the condition's class, by name or SQLSTATE.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_exc_class_app;
         CREATE TABLE qp_exc_class_t (id int);
         CREATE TABLE qp_exc_class_audit (v int);
         GRANT SELECT, INSERT ON qp_exc_class_t TO qp_exc_class_app;
         CREATE FUNCTION qp_exc_class_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           BEGIN
             INSERT INTO qp_exc_class_audit VALUES (NEW.id);
           EXCEPTION WHEN syntax_error_or_access_rule_violation THEN NULL;
           END;
           BEGIN
             DELETE FROM qp_exc_class_audit;
           EXCEPTION WHEN SQLSTATE '42000' THEN NULL;
           END;
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_exc_class_trg AFTER INSERT ON qp_exc_class_t
             FOR EACH ROW EXECUTE FUNCTION qp_exc_class_trg();",
    )]);
    allowed(
        &db,
        "qp_exc_class_app",
        "INSERT INTO qp_exc_class_t VALUES (1)",
    );

    // `WHEN OTHERS` catches it too, and the handler's own statements are
    // still checked.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_exc_others_app;
         CREATE TABLE qp_exc_others_t (id int);
         CREATE TABLE qp_exc_others_audit (v int);
         CREATE TABLE qp_exc_others_hidden (v int);
         GRANT SELECT, INSERT ON qp_exc_others_t TO qp_exc_others_app;
         CREATE FUNCTION qp_exc_others_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           BEGIN
             INSERT INTO qp_exc_others_audit VALUES (NEW.id);
           EXCEPTION WHEN OTHERS THEN
             INSERT INTO qp_exc_others_hidden VALUES (NEW.id);
           END;
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_exc_others_trg AFTER INSERT ON qp_exc_others_t
             FOR EACH ROW EXECUTE FUNCTION qp_exc_others_trg();",
    )]);
    denied_with(
        &db,
        "qp_exc_others_app",
        "INSERT INTO qp_exc_others_t VALUES (1)",
        "permission denied for table qp_exc_others_hidden",
    );
}

#[test]
fn statements_after_return_or_raise_and_constant_branches_are_not_checked() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_dead_app;
         CREATE TABLE qp_dead_t (id int);
         CREATE TABLE qp_dead_ok (v int);
         CREATE TABLE qp_dead_hidden (v int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_dead_t TO qp_dead_app;
         GRANT INSERT ON qp_dead_ok TO qp_dead_app;
         CREATE FUNCTION qp_dead_ret() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO qp_dead_ok VALUES (NEW.id);
           RETURN NEW;
           INSERT INTO qp_dead_hidden VALUES (NEW.id);
         END $$;
         CREATE TRIGGER qp_dead_ret_trg AFTER INSERT ON qp_dead_t
             FOR EACH ROW EXECUTE FUNCTION qp_dead_ret();
         CREATE FUNCTION qp_dead_raise() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           RAISE EXCEPTION 'stop';
           INSERT INTO qp_dead_hidden VALUES (NEW.id);
         END $$;
         CREATE TRIGGER qp_dead_raise_trg AFTER UPDATE ON qp_dead_t
             FOR EACH ROW EXECUTE FUNCTION qp_dead_raise();
         CREATE FUNCTION qp_dead_branches() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF false THEN
             INSERT INTO qp_dead_hidden VALUES (1);
           ELSIF true THEN
             INSERT INTO qp_dead_ok VALUES (NEW.id);
           ELSE
             INSERT INTO qp_dead_hidden VALUES (2);
           END IF;
           WHILE NULL LOOP
             INSERT INTO qp_dead_hidden VALUES (3);
           END LOOP;
           CASE WHEN false THEN
             INSERT INTO qp_dead_hidden VALUES (4);
           ELSE
             NULL;
           END CASE;
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_dead_branches_trg AFTER DELETE ON qp_dead_t
             FOR EACH ROW EXECUTE FUNCTION qp_dead_branches();",
    )]);
    allowed(&db, "qp_dead_app", "INSERT INTO qp_dead_t VALUES (1)");
    allowed(&db, "qp_dead_app", "UPDATE qp_dead_t SET id = 2");
    allowed(&db, "qp_dead_app", "DELETE FROM qp_dead_t");
}

#[test]
fn data_dependent_trigger_when_stays_checked() {
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_datawhen_app;
         CREATE TABLE qp_datawhen_t (id int, x int);
         CREATE TABLE qp_datawhen_audit (v int);
         INSERT INTO qp_datawhen_t VALUES (1, 1);
         CREATE FUNCTION qp_datawhen_trg() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_datawhen_audit VALUES (NEW.id); RETURN NEW; END $$;
         CREATE TRIGGER qp_datawhen_trg AFTER UPDATE ON qp_datawhen_t
             FOR EACH ROW WHEN (OLD.id IS DISTINCT FROM NEW.id)
             EXECUTE FUNCTION qp_datawhen_trg();
         GRANT SELECT, UPDATE ON qp_datawhen_t TO qp_datawhen_app;",
    )]);
    // A WHEN the data decides stays checked (privileges.rs's module doc).
    db.skip_pg_sanity();
    denied_with(
        &db,
        "qp_datawhen_app",
        "UPDATE qp_datawhen_t SET x = 5",
        "permission denied for table qp_datawhen_audit",
    );
}

#[test]
fn plpgsql_queries_in_every_statement_kind_are_checked() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_kinds_app;
         CREATE TABLE qp_kinds_t (id int PRIMARY KEY, x int);
         CREATE TABLE qp_kinds_denied (id int, x int);
         INSERT INTO qp_kinds_t VALUES (1, 1);
         GRANT SELECT, INSERT, UPDATE ON qp_kinds_t TO qp_kinds_app;
         CREATE FUNCTION qp_kinds_1(p int) RETURNS SETOF int LANGUAGE plpgsql AS $$
         BEGIN PERFORM 1 FROM qp_kinds_denied; END $$;
         CREATE FUNCTION qp_kinds_2() RETURNS SETOF int LANGUAGE plpgsql AS $$
         BEGIN RETURN QUERY SELECT id FROM qp_kinds_denied; END $$;
         CREATE FUNCTION qp_kinds_3() RETURNS SETOF int LANGUAGE plpgsql AS $$
         DECLARE r record;
         BEGIN FOR r IN SELECT id FROM qp_kinds_denied LOOP RETURN NEXT r.id; END LOOP; END $$;
         CREATE FUNCTION qp_kinds_4() RETURNS SETOF int LANGUAGE plpgsql AS $$
         DECLARE n int;
         BEGIN SELECT id INTO n FROM qp_kinds_denied; END $$;
         CREATE FUNCTION qp_kinds_5() RETURNS SETOF int LANGUAGE plpgsql AS $$
         DECLARE n int;
         BEGIN n := (SELECT count(*) FROM qp_kinds_denied); END $$;
         CREATE FUNCTION qp_kinds_6() RETURNS SETOF int LANGUAGE plpgsql AS $$
         BEGIN IF EXISTS (SELECT 1 FROM qp_kinds_denied) THEN RETURN; END IF; END $$;
         CREATE FUNCTION qp_kinds_7() RETURNS SETOF int LANGUAGE plpgsql AS $$
         DECLARE r record;
         BEGIN FOR r IN SELECT id FROM qp_kinds_t LOOP
           UPDATE qp_kinds_denied SET x = r.id WHERE id = r.id;
         END LOOP; END $$;
         CREATE FUNCTION qp_kinds_8() RETURNS SETOF int LANGUAGE plpgsql AS $$
         DECLARE n int;
         BEGIN WITH d AS (SELECT id FROM qp_kinds_denied)
           SELECT count(*) INTO n FROM d; END $$;
         CREATE FUNCTION qp_kinds_9() RETURNS SETOF int LANGUAGE plpgsql AS $$
         BEGIN FOR i IN 1..(SELECT count(*) FROM qp_kinds_denied) LOOP END LOOP; END $$;
         CREATE FUNCTION qp_kinds_10() RETURNS SETOF int LANGUAGE plpgsql AS $$
         BEGIN RETURN QUERY SELECT x FROM qp_kinds_t
           UNION SELECT x FROM qp_kinds_denied; END $$;
         CREATE FUNCTION qp_kinds_11(p int) RETURNS SETOF int LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO qp_kinds_t VALUES (p, p)
           ON CONFLICT (id) DO UPDATE SET x = (SELECT max(x) FROM qp_kinds_denied);
         END $$;
         CREATE FUNCTION qp_kinds_12(p int) RETURNS SETOF int LANGUAGE plpgsql AS $$
         DECLARE n int;
         BEGIN RAISE NOTICE '%', (SELECT count(*) FROM qp_kinds_denied); END $$;
         CREATE FUNCTION qp_kinds_13(p int) RETURNS SETOF int LANGUAGE plpgsql AS $$
         DECLARE n int;
         BEGIN WHILE p > 0 LOOP
           EXIT WHEN (SELECT count(*) FROM qp_kinds_denied) > 0;
         END LOOP; END $$;
         CREATE FUNCTION qp_kinds_14(p int) RETURNS SETOF int LANGUAGE plpgsql AS $$
         BEGIN FOREACH p IN ARRAY (SELECT array_agg(id) FROM qp_kinds_denied) LOOP END LOOP; END $$;
         GRANT EXECUTE ON FUNCTION qp_kinds_1(int), qp_kinds_2(), qp_kinds_3(),
             qp_kinds_4(), qp_kinds_5(), qp_kinds_6(), qp_kinds_7(),
             qp_kinds_8(), qp_kinds_9(), qp_kinds_10(), qp_kinds_11(int),
             qp_kinds_12(int), qp_kinds_13(int), qp_kinds_14(int)
             TO qp_kinds_app;",
    )]);
    for sql in [
        "SELECT * FROM qp_kinds_1(1)",
        "SELECT * FROM qp_kinds_2()",
        "SELECT * FROM qp_kinds_3()",
        "SELECT * FROM qp_kinds_4()",
        "SELECT * FROM qp_kinds_5()",
        "SELECT * FROM qp_kinds_6()",
        "SELECT * FROM qp_kinds_7()",
        "SELECT * FROM qp_kinds_8()",
        "SELECT * FROM qp_kinds_9()",
        "SELECT * FROM qp_kinds_10()",
        "SELECT * FROM qp_kinds_11(1)",
        "SELECT * FROM qp_kinds_12(1)",
        "SELECT * FROM qp_kinds_13(1)",
        "SELECT * FROM qp_kinds_14(1)",
    ] {
        denied_with(
            &db,
            "qp_kinds_app",
            sql,
            "permission denied for table qp_kinds_denied",
        );
    }
}

#[test]
fn plpgsql_constructs_without_denials_are_allowed() {
    // Every statement kind a PL/pgSQL body can hold, all of it granted:
    // the walker must not report a denial.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_okpl_app;
         CREATE TABLE qp_okpl_t (id int PRIMARY KEY, x int, name text);
         INSERT INTO qp_okpl_t VALUES (1, 1, 'a');
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_okpl_t TO qp_okpl_app;
         CREATE TYPE qp_okpl_pair AS (a int, b int);
         CREATE FUNCTION qp_okpl_f(p_id int, OUT total bigint, OUT names text)
             LANGUAGE plpgsql AS $$
         <<blk>>
         DECLARE
           v qp_okpl_t.x%TYPE := (SELECT max(x) FROM qp_okpl_t);
           r qp_okpl_t%ROWTYPE;
           rec record;
           pr qp_okpl_pair;
           c CURSOR (k int) FOR SELECT * FROM qp_okpl_t WHERE id = k;
           n int := 0;
         BEGIN
           SELECT * INTO r FROM qp_okpl_t WHERE id = p_id;
           SELECT * INTO STRICT rec FROM qp_okpl_t WHERE id = p_id;
           pr := ROW(1, 2);
           n = n + pr.a + r.x + rec.x + blk.n;
           FOR rec IN SELECT id, x FROM qp_okpl_t WHERE x > v LOOP
             n := n + rec.x;
             UPDATE qp_okpl_t SET x = rec.x + 1 WHERE id = rec.id;
           END LOOP;
           FOREACH n IN ARRAY ARRAY[1,2] LOOP NULL; END LOOP;
           PERFORM 1 FROM qp_okpl_t WHERE id = qp_okpl_f.p_id;
           EXECUTE format('SELECT count(*) FROM %I', 'qp_okpl_t') INTO total;
           EXECUTE 'SELECT $1' INTO n USING (SELECT count(*) FROM qp_okpl_t);
           OPEN c(p_id); CLOSE c;
           GET DIAGNOSTICS n = ROW_COUNT;
           IF NOT FOUND THEN RAISE NOTICE 'x %', (SELECT count(*) FROM qp_okpl_t); END IF;
           INSERT INTO qp_okpl_t AS tt VALUES (p_id + 100, 1, 'b')
             ON CONFLICT (id) DO UPDATE SET x = tt.x + 1 RETURNING tt.x INTO n;
           total := (SELECT count(*) FROM qp_okpl_t);
           names := (SELECT string_agg(name, ',') FROM qp_okpl_t);
           BEGIN
             DELETE FROM qp_okpl_t WHERE id = -1;
           EXCEPTION WHEN others THEN GET STACKED DIAGNOSTICS names = MESSAGE_TEXT;
           END;
           CASE WHEN n > 0 THEN n := 1; ELSE n := 2; END CASE;
           WHILE n < 3 LOOP n := n + 1; EXIT WHEN n > 10; END LOOP;
           ASSERT n > 0;
         END $$;
         GRANT EXECUTE ON FUNCTION qp_okpl_f(int) TO qp_okpl_app;",
    )]);
    allowed(&db, "qp_okpl_app", "SELECT * FROM qp_okpl_f(1)");
}

#[test]
fn exception_handler_statements_are_checked_when_unreached() {
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_unreached_app;
         CREATE TABLE qp_unreached_denied (id int);
         CREATE FUNCTION qp_unreached_f() RETURNS SETOF int LANGUAGE plpgsql AS $$
         BEGIN
           BEGIN
             RETURN;
           EXCEPTION WHEN others THEN
             PERFORM 1 FROM qp_unreached_denied;
           END;
         END $$;
         GRANT EXECUTE ON FUNCTION qp_unreached_f() TO qp_unreached_app;",
    )]);
    // Handler statements stay checked (privileges.rs's module doc).
    db.skip_pg_sanity();
    denied_with(
        &db,
        "qp_unreached_app",
        "SELECT * FROM qp_unreached_f()",
        "permission denied for table qp_unreached_denied",
    );
}

// ─── Role/ACL model, second round: query-observed cases ─────────────────────

#[test]
fn revoking_a_table_privilege_also_revokes_the_implied_column_grant() {
    // PG expands a table-level REVOKE's column bits before restricting them
    // by the relation-level grant options, then revokes each column with the
    // grantor chosen from the combined table + column ACLs.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_colrev_app;
         CREATE ROLE qp_colrev_b;
         CREATE TABLE qp_colrev_t (id int);
         GRANT SELECT ON qp_colrev_t TO qp_colrev_app;
         GRANT SELECT (id) ON qp_colrev_t TO qp_colrev_app WITH GRANT OPTION;
         SET ROLE qp_colrev_app;
         GRANT SELECT (id) ON qp_colrev_t TO qp_colrev_b;
         REVOKE SELECT ON qp_colrev_t FROM qp_colrev_b;
         RESET ROLE;",
    )]);
    assert!(!db.has_any_column_privilege(
        "qp_colrev_b",
        crate::common::class_oid(&db, Some("public"), "qp_colrev_t"),
        typedpg_analyzer::Privilege::Select
    ));
    denied_with(
        &db,
        "qp_colrev_b",
        "SELECT id FROM qp_colrev_t",
        "permission denied for table qp_colrev_t",
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Second round of statement / body checks (review findings F1–F10, M1–M13)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn view_writes_do_not_run_the_select_list() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_view_app;
         CREATE FUNCTION qp_r2_view_f(int) RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_view_f(int) FROM PUBLIC;
         CREATE TABLE qp_r2_view_t (id int, v int);
         INSERT INTO qp_r2_view_t VALUES (1, 1);
         CREATE VIEW qp_r2_view_v AS SELECT id, qp_r2_view_f(v) AS fv FROM qp_r2_view_t;
         GRANT ALL ON qp_r2_view_v TO qp_r2_view_app;",
    )]);
    // UPDATE / DELETE evaluate only the view's WHERE; INSERT only its WITH
    // CHECK qual: the select list's function never runs.
    allowed(&db, "qp_r2_view_app", "UPDATE qp_r2_view_v SET id = 2");
    allowed(&db, "qp_r2_view_app", "DELETE FROM qp_r2_view_v");
    allowed(
        &db,
        "qp_r2_view_app",
        "INSERT INTO qp_r2_view_v (id) VALUES (1)",
    );
    // Reading the expression column still runs it.
    denied_with(
        &db,
        "qp_r2_view_app",
        "SELECT fv FROM qp_r2_view_v",
        "permission denied for function qp_r2_view_f",
    );
    // A WHERE that reads it (the statement's own, or RETURNING) does too.
    denied_with(
        &db,
        "qp_r2_view_app",
        "UPDATE qp_r2_view_v SET id = 2 WHERE fv = 1",
        "permission denied for function qp_r2_view_f",
    );
    denied_with(
        &db,
        "qp_r2_view_app",
        "DELETE FROM qp_r2_view_v RETURNING fv",
        "permission denied for function qp_r2_view_f",
    );

    // The view's WHERE still runs: A2's denials keep working.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_vw_app;
         CREATE FUNCTION qp_r2_vw_f(int) RETURNS bool LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_vw_f(int) FROM PUBLIC;
         CREATE TABLE qp_r2_vw_t (id int, v int);
         INSERT INTO qp_r2_vw_t VALUES (1, 1);
         CREATE VIEW qp_r2_vw_v AS SELECT id FROM qp_r2_vw_t WHERE qp_r2_vw_f(v);
         GRANT ALL ON qp_r2_vw_v TO qp_r2_vw_app;",
    )]);
    denied_with(
        &db,
        "qp_r2_vw_app",
        "UPDATE qp_r2_vw_v SET id = 2",
        "permission denied for function qp_r2_vw_f",
    );
    denied_with(
        &db,
        "qp_r2_vw_app",
        "DELETE FROM qp_r2_vw_v",
        "permission denied for function qp_r2_vw_f",
    );
}

#[test]
fn cascaded_check_option_propagates_to_lower_views() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_casc_app;
         CREATE FUNCTION qp_r2_casc_f(int) RETURNS bool LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_casc_f(int) FROM PUBLIC;
         CREATE TABLE qp_r2_casc_t (id int, v int);
         CREATE VIEW qp_r2_casc_v1 AS SELECT id, v FROM qp_r2_casc_t WHERE qp_r2_casc_f(v);
         CREATE VIEW qp_r2_casc_v2 AS SELECT id, v FROM qp_r2_casc_v1
             WHERE v > 0 WITH CASCADED CHECK OPTION;
         GRANT INSERT ON qp_r2_casc_v2 TO qp_r2_casc_app;",
    )]);
    // CASCADED checks v1's WHERE too, on INSERT.
    denied_with(
        &db,
        "qp_r2_casc_app",
        "INSERT INTO qp_r2_casc_v2 VALUES (1, 1)",
        "permission denied for function qp_r2_casc_f",
    );
}

#[test]
fn local_check_option_does_not_propagate() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_local_app;
         CREATE FUNCTION qp_r2_local_f(int) RETURNS bool LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_local_f(int) FROM PUBLIC;
         CREATE TABLE qp_r2_local_t (id int, v int);
         CREATE VIEW qp_r2_local_v1 AS SELECT id, v FROM qp_r2_local_t
             WHERE qp_r2_local_f(v);
         CREATE VIEW qp_r2_local_v2 AS SELECT id, v FROM qp_r2_local_v1
             WHERE v > 0 WITH LOCAL CHECK OPTION;
         GRANT INSERT ON qp_r2_local_v2 TO qp_r2_local_app;",
    )]);
    // LOCAL checks v2's own WHERE only.
    allowed(
        &db,
        "qp_r2_local_app",
        "INSERT INTO qp_r2_local_v2 VALUES (1, 1)",
    );
}

#[test]
fn inner_check_option_still_applies_under_a_plain_view() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_inner_app;
         CREATE FUNCTION qp_r2_inner_f(int) RETURNS bool LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_inner_f(int) FROM PUBLIC;
         CREATE TABLE qp_r2_inner_t (id int, v int);
         CREATE VIEW qp_r2_inner_v1 AS SELECT id, v FROM qp_r2_inner_t
             WHERE qp_r2_inner_f(v) WITH LOCAL CHECK OPTION;
         CREATE VIEW qp_r2_inner_v2 AS SELECT id, v FROM qp_r2_inner_v1 WHERE v > 0;
         GRANT INSERT ON qp_r2_inner_v2 TO qp_r2_inner_app;",
    )]);
    // v2 has no option, but the lower view's own CHECK OPTION applies.
    denied_with(
        &db,
        "qp_r2_inner_app",
        "INSERT INTO qp_r2_inner_v2 VALUES (1, 1)",
        "permission denied for function qp_r2_inner_f",
    );
}

#[test]
fn update_may_move_a_row_through_a_before_trigger() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_move_app;
         CREATE TABLE qp_r2_move_secret (x int);
         CREATE TABLE qp_r2_move_p (k int, v int) PARTITION BY LIST (k);
         CREATE TABLE qp_r2_move_p1 PARTITION OF qp_r2_move_p FOR VALUES IN (1);
         CREATE TABLE qp_r2_move_p2 PARTITION OF qp_r2_move_p FOR VALUES IN (2);
         INSERT INTO qp_r2_move_p VALUES (1, 1);
         CREATE FUNCTION qp_r2_move_bk() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN NEW.k := 2; RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_move_bk BEFORE UPDATE ON qp_r2_move_p
             FOR EACH ROW EXECUTE FUNCTION qp_r2_move_bk();
         CREATE FUNCTION qp_r2_move_ins() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN INSERT INTO qp_r2_move_secret VALUES (1); RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_move_ins AFTER INSERT ON qp_r2_move_p2
             FOR EACH ROW EXECUTE FUNCTION qp_r2_move_ins();
         GRANT ALL ON qp_r2_move_p TO qp_r2_move_app;",
    )]);
    // The SET list assigns no key column, but the BEFORE trigger may: the
    // row moves and p2's INSERT trigger fires.
    denied_with(
        &db,
        "qp_r2_move_app",
        "UPDATE qp_r2_move_p SET v = 5",
        "permission denied for table qp_r2_move_secret",
    );
}

#[test]
fn index_expressions_run_for_a_non_hot_update() {
    // A partial index whose predicate reads the updated column prevents a
    // HOT update, so PG evaluates it: an oracle-compatible denial.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_hot_app;
         CREATE FUNCTION qp_r2_hot_f(int) RETURNS bool LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_hot_f(int) FROM PUBLIC;
         CREATE TABLE qp_r2_hot_t (id int, b int);
         INSERT INTO qp_r2_hot_t VALUES (1, 1);
         CREATE INDEX qp_r2_hot_pred ON qp_r2_hot_t (id) WHERE qp_r2_hot_f(b);
         GRANT UPDATE (b) ON qp_r2_hot_t TO qp_r2_hot_app;",
    )]);
    denied_with(
        &db,
        "qp_r2_hot_app",
        "UPDATE qp_r2_hot_t SET b = 2",
        "permission denied for function qp_r2_hot_f",
    );
}

#[test]
fn domain_checks_reach_array_and_composite_values() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_dom_app;
         CREATE FUNCTION qp_r2_dom_f(int) RETURNS bool LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_dom_f(int) FROM PUBLIC;
         CREATE DOMAIN qp_r2_dom_d AS int CHECK (qp_r2_dom_f(VALUE));
         CREATE DOMAIN qp_r2_dom_da AS qp_r2_dom_d[];
         CREATE TYPE qp_r2_dom_c AS (x qp_r2_dom_d);
         CREATE TABLE qp_r2_dom_arr (a qp_r2_dom_d[]);
         CREATE TABLE qp_r2_dom_arrdom (a qp_r2_dom_da);
         CREATE TABLE qp_r2_dom_comp (a qp_r2_dom_c);
         GRANT ALL ON qp_r2_dom_arr, qp_r2_dom_arrdom, qp_r2_dom_comp
             TO qp_r2_dom_app;",
    )]);
    denied_with(
        &db,
        "qp_r2_dom_app",
        "INSERT INTO qp_r2_dom_arr VALUES (ARRAY[1])",
        "permission denied for function qp_r2_dom_f",
    );
    denied_with(
        &db,
        "qp_r2_dom_app",
        "INSERT INTO qp_r2_dom_arrdom VALUES (ARRAY[1])",
        "permission denied for function qp_r2_dom_f",
    );
    denied_with(
        &db,
        "qp_r2_dom_app",
        "INSERT INTO qp_r2_dom_comp VALUES (ROW(1))",
        "permission denied for function qp_r2_dom_f",
    );
}

#[test]
fn null_array_of_domain_elements_runs_no_element_check() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_nullarr_app;
         CREATE FUNCTION qp_r2_nullarr_f(int) RETURNS int LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_nullarr_f(int) FROM PUBLIC;
         CREATE DOMAIN qp_r2_nullarr_d AS int CHECK (qp_r2_nullarr_f(VALUE) = 1);
         CREATE TABLE qp_r2_nullarr_t (id int, a qp_r2_nullarr_d[]);
         GRANT ALL ON qp_r2_nullarr_t TO qp_r2_nullarr_app;",
    )]);
    // The omitted column is a NULL array: no element exists to check.
    allowed(
        &db,
        "qp_r2_nullarr_app",
        "INSERT INTO qp_r2_nullarr_t (id) VALUES (1)",
    );
}

#[test]
fn update_of_a_virtual_generated_column_fires() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_virt_app;
         CREATE TABLE qp_r2_virt_secret (x int);
         CREATE TABLE qp_r2_virt_t (a int, g int GENERATED ALWAYS AS (a * 2) VIRTUAL);
         INSERT INTO qp_r2_virt_t VALUES (1);
         CREATE FUNCTION qp_r2_virt_f() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN INSERT INTO qp_r2_virt_secret VALUES (1); RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_virt_trg BEFORE UPDATE OF g ON qp_r2_virt_t
             FOR EACH ROW EXECUTE FUNCTION qp_r2_virt_f();
         GRANT ALL ON qp_r2_virt_t TO qp_r2_virt_app;",
    )]);
    // g is virtual and reads a: updating a "updates" g too.
    denied_with(
        &db,
        "qp_r2_virt_app",
        "UPDATE qp_r2_virt_t SET a = 2",
        "permission denied for table qp_r2_virt_secret",
    );
}

#[test]
fn sequence_defaults_are_found_through_every_expression_kind() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_seq_app;
         CREATE SEQUENCE qp_r2_seq_s;
         CREATE TABLE qp_r2_seq_minmax (a bigint DEFAULT GREATEST(nextval('qp_r2_seq_s'), 0));
         CREATE TABLE qp_r2_seq_nulltest (a bigint
             DEFAULT CASE WHEN nextval('qp_r2_seq_s') IS NOT NULL THEN 1 END);
         CREATE TABLE qp_r2_seq_coalesce (a bigint DEFAULT COALESCE(nextval('qp_r2_seq_s'), 0));
         CREATE TABLE qp_r2_seq_booltest (a int
             DEFAULT (nextval('qp_r2_seq_s') > 0)::int);
         CREATE TABLE qp_r2_seq_array (a bigint[] DEFAULT ARRAY[nextval('qp_r2_seq_s')]);
         CREATE TABLE qp_r2_seq_row (a bigint DEFAULT (ROW(nextval('qp_r2_seq_s'))).f1);
         GRANT ALL ON qp_r2_seq_minmax, qp_r2_seq_nulltest, qp_r2_seq_coalesce,
             qp_r2_seq_booltest, qp_r2_seq_array, qp_r2_seq_row TO qp_r2_seq_app;",
    )]);
    for sql in [
        "INSERT INTO qp_r2_seq_minmax DEFAULT VALUES",
        "INSERT INTO qp_r2_seq_nulltest DEFAULT VALUES",
        "INSERT INTO qp_r2_seq_coalesce DEFAULT VALUES",
        "INSERT INTO qp_r2_seq_booltest DEFAULT VALUES",
        "INSERT INTO qp_r2_seq_array DEFAULT VALUES",
        "INSERT INTO qp_r2_seq_row DEFAULT VALUES",
    ] {
        denied_with(
            &db,
            "qp_r2_seq_app",
            sql,
            "permission denied for sequence qp_r2_seq_s",
        );
    }
}

#[test]
fn record_assignments_and_rowtype_variables_get_their_shape() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_rec_app;
         CREATE TABLE qp_r2_rec_secret (x int);
         CREATE TABLE qp_r2_rec_1 (id int, x int);
         CREATE TABLE qp_r2_rec_2 (id int, x int);
         CREATE TABLE qp_r2_rec_3 (id int, x int);
         CREATE TABLE qp_r2_rec_4 (id int, x int);
         INSERT INTO qp_r2_rec_1 VALUES (5, 5), (6, 6);
         INSERT INTO qp_r2_rec_2 VALUES (7, 7);
         INSERT INTO qp_r2_rec_3 VALUES (8, 8);
         INSERT INTO qp_r2_rec_4 VALUES (9, 9);
         GRANT SELECT, INSERT, UPDATE, DELETE ON
             qp_r2_rec_1, qp_r2_rec_2, qp_r2_rec_3, qp_r2_rec_4 TO qp_r2_rec_app;
         CREATE FUNCTION qp_r2_rec_f1() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE r record; BEGIN r := NEW; INSERT INTO qp_r2_rec_secret VALUES (r.x);
           RETURN NEW; END $$;
         CREATE FUNCTION qp_r2_rec_f2() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE r qp_r2_rec_2; BEGIN r := NEW;
           INSERT INTO qp_r2_rec_secret VALUES (r.x); RETURN NEW; END $$;
         CREATE FUNCTION qp_r2_rec_f3() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE r qp_r2_rec_3%ROWTYPE; BEGIN r := NEW;
           INSERT INTO qp_r2_rec_secret VALUES (r.x); RETURN NEW; END $$;
         CREATE FUNCTION qp_r2_rec_f4() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE r record; BEGIN SELECT NEW.* INTO r;
           INSERT INTO qp_r2_rec_secret VALUES (r.x); RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_rec_1_trg BEFORE INSERT ON qp_r2_rec_1 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_rec_f1();
         CREATE TRIGGER qp_r2_rec_2_trg BEFORE INSERT ON qp_r2_rec_2 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_rec_f2();
         CREATE TRIGGER qp_r2_rec_3_trg BEFORE INSERT ON qp_r2_rec_3 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_rec_f3();
         CREATE TRIGGER qp_r2_rec_4_trg BEFORE INSERT ON qp_r2_rec_4 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_rec_f4();",
    )]);
    for table in ["qp_r2_rec_1", "qp_r2_rec_2", "qp_r2_rec_3", "qp_r2_rec_4"] {
        denied_with(
            &db,
            "qp_r2_rec_app",
            &format!("INSERT INTO {table} VALUES (1, 2)"),
            "permission denied for table qp_r2_rec_secret",
        );
    }
}

#[test]
fn a_deferred_constraint_trigger_is_not_caught() {
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_def_app;
         CREATE TABLE qp_r2_def_secret (x int);
         CREATE TABLE qp_r2_def_t (id int);
         CREATE TABLE qp_r2_def_t2 (id int);
         GRANT ALL ON qp_r2_def_t, qp_r2_def_t2 TO qp_r2_def_app;
         CREATE FUNCTION qp_r2_def_d() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN INSERT INTO qp_r2_def_secret VALUES (1); RETURN NULL; END $$;
         CREATE CONSTRAINT TRIGGER qp_r2_def_d AFTER INSERT ON qp_r2_def_t2
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION qp_r2_def_d();
         CREATE FUNCTION qp_r2_def_tf() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN BEGIN INSERT INTO qp_r2_def_t2 VALUES (1);
           EXCEPTION WHEN others THEN NULL; END; RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_def_trg BEFORE INSERT ON qp_r2_def_t
             FOR EACH ROW EXECUTE FUNCTION qp_r2_def_tf();",
    )]);
    // The deferred trigger runs at commit, after the handler's block: the
    // handler doesn't catch its denial. The oracle rolls the transaction
    // back and so never fires it (policy: a deferrable trigger may fire).
    db.skip_pg_sanity();
    denied_with(
        &db,
        "qp_r2_def_app",
        "INSERT INTO qp_r2_def_t VALUES (1)",
        "permission denied for table qp_r2_def_secret",
    );
}

#[test]
fn unknown_tg_variables_are_not_null() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_tg_app;
         CREATE TABLE qp_r2_tg_secret (x int);
         CREATE TABLE qp_r2_tg_1 (id int, x int);
         CREATE TABLE qp_r2_tg_2 (id int, x int);
         CREATE TABLE qp_r2_tg_3 (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON
             qp_r2_tg_1, qp_r2_tg_2, qp_r2_tg_3 TO qp_r2_tg_app;
         CREATE FUNCTION qp_r2_tg_name2() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF TG_NAME <> 'zzz' THEN INSERT INTO qp_r2_tg_secret VALUES (1);
           END IF; RETURN NEW; END $$;
         CREATE FUNCTION qp_r2_tg_nargs() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF TG_NARGS = 1 THEN INSERT INTO qp_r2_tg_secret VALUES (1);
           END IF; RETURN NEW; END $$;
         CREATE FUNCTION qp_r2_tg_argw() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF TG_ARGV IS NOT NULL THEN
             INSERT INTO qp_r2_tg_secret VALUES (1);
           END IF; RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_tg_1_trg BEFORE INSERT ON qp_r2_tg_1 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_tg_name2();
         CREATE TRIGGER qp_r2_tg_2_trg BEFORE INSERT ON qp_r2_tg_2 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_tg_nargs('x');
         CREATE TRIGGER qp_r2_tg_3_trg BEFORE INSERT ON qp_r2_tg_3 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_tg_argw('x');",
    )]);
    // TG_NAME is known (the trigger is `qp_r2_tg_1_trg`), so the condition
    // is certainly true.
    denied_with(
        &db,
        "qp_r2_tg_app",
        "INSERT INTO qp_r2_tg_1 VALUES (1, 2)",
        "permission denied for table qp_r2_tg_secret",
    );
    // TG_NARGS / TG_ARGV aren't evaluated: UNKNOWN never folds a branch.
    for table in ["qp_r2_tg_2", "qp_r2_tg_3"] {
        denied_with(
            &db,
            "qp_r2_tg_app",
            &format!("INSERT INTO {table} VALUES (1, 2)"),
            "permission denied for table qp_r2_tg_secret",
        );
    }
}

#[test]
fn plpgsql_user_variables_named_tg_are_not_tg_variables() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_tgvar_app;
         CREATE TABLE qp_r2_tgvar_secret (x int);
         CREATE TABLE qp_r2_tgvar_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_r2_tgvar_t TO qp_r2_tgvar_app;
         CREATE FUNCTION qp_r2_tgvar() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE tg_mode text := 'a';
         BEGIN IF tg_mode = 'a' THEN INSERT INTO qp_r2_tgvar_secret VALUES (1);
           END IF; RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_tgvar_trg BEFORE INSERT ON qp_r2_tgvar_t FOR EACH ROW
           EXECUTE FUNCTION qp_r2_tgvar();",
    )]);
    denied_with(
        &db,
        "qp_r2_tgvar_app",
        "INSERT INTO qp_r2_tgvar_t VALUES (1, 2)",
        "permission denied for table qp_r2_tgvar_secret",
    );
}

#[test]
fn a_reraising_handler_does_not_catch_the_denial() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_raise_app;
         CREATE TABLE qp_r2_raise_secret (x int);
         CREATE TABLE qp_r2_raise_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_r2_raise_t TO qp_r2_raise_app;
         CREATE FUNCTION qp_r2_raise() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN BEGIN INSERT INTO qp_r2_raise_secret VALUES (1);
           EXCEPTION WHEN others THEN RAISE; END; RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_raise_trg BEFORE INSERT ON qp_r2_raise_t FOR EACH ROW
           EXECUTE FUNCTION qp_r2_raise();",
    )]);
    denied_with(
        &db,
        "qp_r2_raise_app",
        "INSERT INTO qp_r2_raise_t VALUES (1, 2)",
        "permission denied for table qp_r2_raise_secret",
    );
}

#[test]
fn pruned_exception_bodies_still_give_their_records_a_shape() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_shape_app;
         CREATE TABLE qp_r2_shape_secret (x int);
         CREATE TABLE qp_r2_shape_t (id int, x int);
         INSERT INTO qp_r2_shape_t VALUES (5, 5);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_r2_shape_t TO qp_r2_shape_app;
         CREATE FUNCTION qp_r2_shape() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE r record; BEGIN
           BEGIN SELECT id, x INTO r FROM qp_r2_shape_t LIMIT 1;
           EXCEPTION WHEN others THEN NULL; END;
           INSERT INTO qp_r2_shape_secret VALUES (r.x); RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_shape_trg BEFORE INSERT ON qp_r2_shape_t FOR EACH ROW
           EXECUTE FUNCTION qp_r2_shape();",
    )]);
    // The handler catches the body, but `r`'s shape (from its SELECT INTO)
    // must still be known so the later INSERT analyzes.
    denied_with(
        &db,
        "qp_r2_shape_app",
        "INSERT INTO qp_r2_shape_t VALUES (1, 2)",
        "permission denied for table qp_r2_shape_secret",
    );
}

#[test]
fn typed_comparisons_are_not_folded_as_strings() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_str_app;
         CREATE TABLE qp_r2_str_secret (x int);
         CREATE TABLE qp_r2_str_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_r2_str_t TO qp_r2_str_app;
         CREATE FUNCTION qp_r2_str() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF '1'::int = '01'::int THEN
             INSERT INTO qp_r2_str_secret VALUES (1); END IF; RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_str_trg BEFORE INSERT ON qp_r2_str_t FOR EACH ROW
           EXECUTE FUNCTION qp_r2_str();",
    )]);
    // The typed comparison folds to true (1 = 1): the branch runs.
    denied_with(
        &db,
        "qp_r2_str_app",
        "INSERT INTO qp_r2_str_t VALUES (1, 2)",
        "permission denied for table qp_r2_str_secret",
    );
}

#[test]
fn domains_and_mutable_casts_are_not_folded() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_fold_app;
         CREATE FUNCTION qp_r2_fold_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_fold_f() FROM PUBLIC;
         CREATE DOMAIN qp_r2_fold_bool AS boolean CHECK (VALUE IS NOT NULL);
         CREATE DOMAIN qp_r2_fold_id AS int CHECK (VALUE > 0);",
    )]);
    // CoerceToDomain is never folded: the ELSE runs.
    denied_with(
        &db,
        "qp_r2_fold_app",
        "SELECT CASE WHEN true::qp_r2_fold_bool THEN 0 ELSE qp_r2_fold_f() END",
        "permission denied for function qp_r2_fold_f",
    );
    denied_with(
        &db,
        "qp_r2_fold_app",
        "SELECT COALESCE(1::qp_r2_fold_id, qp_r2_fold_f())",
        "permission denied for function qp_r2_fold_f",
    );
    // `'now'::timestamptz` is a Const, but its `::text` runs a stable cast
    // function: not folded.
    denied_with(
        &db,
        "qp_r2_fold_app",
        "SELECT COALESCE('now'::timestamptz::text, qp_r2_fold_f()::text)",
        "permission denied for function qp_r2_fold_f",
    );
}

#[test]
fn coalesce_drops_arguments_after_a_certain_constant() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_coal_app;
         CREATE FUNCTION qp_r2_coal_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         CREATE FUNCTION qp_r2_coal_g() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_coal_f() FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_r2_coal_g() FROM PUBLIC;",
    )]);
    // `'2026-01-01'::timestamptz` is a parse-time Const: the rest is dropped.
    allowed(
        &db,
        "qp_r2_coal_app",
        "SELECT COALESCE('2026-01-01'::timestamptz, now() + make_interval(days => qp_r2_coal_f()))",
    );
    // An operator over two constants folds (`1 = 1`), so CASE's ELSE never
    // runs.
    allowed(
        &db,
        "qp_r2_coal_app",
        "SELECT CASE WHEN 1 = 1 THEN 0 ELSE qp_r2_coal_f() END",
    );
    // And `COALESCE(g(), 1, f())` may stop at 1 only past g(): f is dropped
    // but g runs.
    denied_with(
        &db,
        "qp_r2_coal_app",
        "SELECT COALESCE(qp_r2_coal_g(), 1, qp_r2_coal_f())",
        "permission denied for function qp_r2_coal_g",
    );
}

#[test]
fn an_inlined_functions_unused_arguments_do_not_run() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_inline_app;
         CREATE FUNCTION qp_r2_inline_g() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_inline_g() FROM PUBLIC;
         CREATE FUNCTION qp_r2_inline_k(a int, b int DEFAULT qp_r2_inline_g())
             RETURNS int LANGUAGE sql AS $$ SELECT a $$;
         CREATE FUNCTION qp_r2_inline_used(a int, b int DEFAULT qp_r2_inline_g())
             RETURNS int LANGUAGE sql AS $$ SELECT a + b $$;
         CREATE FUNCTION qp_r2_inline_strict(a int, b int DEFAULT qp_r2_inline_g())
             RETURNS int LANGUAGE sql STRICT AS $$ SELECT a $$;
         CREATE FUNCTION qp_r2_inline_twice(a int, b int DEFAULT qp_r2_inline_g())
             RETURNS int LANGUAGE sql AS $$ SELECT a + a $$;",
    )]);
    // Both the explicit argument and the inserted default are dropped: the
    // body doesn't use b.
    allowed(
        &db,
        "qp_r2_inline_app",
        "SELECT qp_r2_inline_k(1, qp_r2_inline_g())",
    );
    allowed(&db, "qp_r2_inline_app", "SELECT qp_r2_inline_k(1)");
    // The body uses b: its default runs.
    denied_with(
        &db,
        "qp_r2_inline_app",
        "SELECT qp_r2_inline_used(1)",
        "permission denied for function qp_r2_inline_g",
    );
    // A cheap argument used twice doesn't keep PG from inlining: b's
    // default is still dropped.
    allowed(&db, "qp_r2_inline_app", "SELECT qp_r2_inline_twice(1)");
    // PG doesn't inline a STRICT function with an unused parameter, so its
    // default runs.
    denied_with(
        &db,
        "qp_r2_inline_app",
        "SELECT qp_r2_inline_strict(1)",
        "permission denied for function qp_r2_inline_g",
    );
}

#[test]
fn stored_expressions_are_folded_before_they_run() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_store_app;
         CREATE FUNCTION qp_r2_store_f(int) RETURNS int LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_store_f(int) FROM PUBLIC;
         CREATE SEQUENCE qp_r2_store_s;
         CREATE TABLE qp_r2_store_chk (a int
             CHECK (CASE WHEN true THEN true ELSE qp_r2_store_f(a) = 1 END));
         CREATE TABLE qp_r2_store_def (id int, a int DEFAULT COALESCE(1, qp_r2_store_f(0)));
         CREATE TABLE qp_r2_store_seq (id int, a bigint
             DEFAULT CASE WHEN false THEN nextval('qp_r2_store_s') ELSE 0 END);
         CREATE TABLE qp_r2_store_idx (a int);
         CREATE INDEX qp_r2_store_idx_expr ON qp_r2_store_idx
             ((CASE WHEN false THEN qp_r2_store_f(a) ELSE a END));
         GRANT ALL ON qp_r2_store_chk, qp_r2_store_def, qp_r2_store_seq,
             qp_r2_store_idx TO qp_r2_store_app;",
    )]);
    allowed(
        &db,
        "qp_r2_store_app",
        "INSERT INTO qp_r2_store_chk VALUES (1)",
    );
    allowed(
        &db,
        "qp_r2_store_app",
        "INSERT INTO qp_r2_store_def (id) VALUES (1)",
    );
    allowed(
        &db,
        "qp_r2_store_app",
        "INSERT INTO qp_r2_store_seq (id) VALUES (1)",
    );
    allowed(
        &db,
        "qp_r2_store_app",
        "INSERT INTO qp_r2_store_idx VALUES (1)",
    );
}

#[test]
fn trigger_when_conditions_are_folded() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_when_app;
         CREATE FUNCTION qp_r2_when_f(int) RETURNS int LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_when_f(int) FROM PUBLIC;
         CREATE TABLE qp_r2_when_secret (x int);
         CREATE TABLE qp_r2_when_t (id int, x int);
         INSERT INTO qp_r2_when_t VALUES (1, 1);
         CREATE FUNCTION qp_r2_when_trg() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN INSERT INTO qp_r2_when_secret VALUES (1); RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_when_1 BEFORE UPDATE ON qp_r2_when_t FOR EACH ROW
             WHEN (1 = 0) EXECUTE FUNCTION qp_r2_when_trg();
         GRANT SELECT, UPDATE ON qp_r2_when_t TO qp_r2_when_app;",
    )]);
    // The WHEN certainly folds to false: the trigger never fires.
    allowed(&db, "qp_r2_when_app", "UPDATE qp_r2_when_t SET x = 2");

    // A short-circuited function is still initialized when the WHEN is
    // prepared (PG checks its EXECUTE), unlike a WHEN that folds away whole.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_when2_app;
         CREATE FUNCTION qp_r2_when2_f(int) RETURNS int LANGUAGE plpgsql
             IMMUTABLE AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_when2_f(int) FROM PUBLIC;
         CREATE TABLE qp_r2_when2_secret (x int);
         CREATE TABLE qp_r2_when2_t (id int, x int);
         INSERT INTO qp_r2_when2_t VALUES (1, 1);
         CREATE FUNCTION qp_r2_when2_trg() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN INSERT INTO qp_r2_when2_secret VALUES (1); RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_when2_1 BEFORE UPDATE ON qp_r2_when2_t FOR EACH ROW
             WHEN (false AND qp_r2_when2_f(NEW.x) = 1)
             EXECUTE FUNCTION qp_r2_when2_trg();
         GRANT SELECT, UPDATE ON qp_r2_when2_t TO qp_r2_when2_app;",
    )]);
    denied_with(
        &db,
        "qp_r2_when2_app",
        "UPDATE qp_r2_when2_t SET x = 2",
        "permission denied for function qp_r2_when2_f",
    );
}

#[test]
fn ctes_referenced_only_by_themselves_or_dead_code_are_not_planned() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_cte_app;
         CREATE TABLE qp_r2_cte_secret (x int);
         INSERT INTO qp_r2_cte_secret VALUES (1);",
    )]);
    // A recursive CTE referenced only by itself is discarded.
    allowed(
        &db,
        "qp_r2_cte_app",
        "WITH RECURSIVE a AS (SELECT 1 AS n UNION ALL \
         SELECT x FROM qp_r2_cte_secret, a WHERE false) SELECT 1",
    );
    // An inlined CTE referenced only from a certainly-false AND argument is
    // never planned (no pull-up: the EXISTS has no WHERE to reference the
    // level).
    allowed(
        &db,
        "qp_r2_cte_app",
        "WITH a AS (SELECT x FROM qp_r2_cte_secret) \
         SELECT 1 FROM (SELECT 1) s WHERE false AND EXISTS (SELECT 1 FROM a)",
    );
    // A MATERIALIZED one is planned whatever reaches it (SS_process_ctes).
    denied_with(
        &db,
        "qp_r2_cte_app",
        "WITH a AS MATERIALIZED (SELECT x FROM qp_r2_cte_secret) \
         SELECT 1 FROM (SELECT 1) s WHERE false AND EXISTS (SELECT 1 FROM a)",
        "permission denied for table qp_r2_cte_secret",
    );
}

#[test]
fn cte_names_shadowed_by_an_inner_with_are_not_referenced() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_shadow_app;
         CREATE TABLE qp_r2_shadow_secret (x int);",
    )]);
    allowed(
        &db,
        "qp_r2_shadow_app",
        "WITH a AS (SELECT * FROM qp_r2_shadow_secret) \
         SELECT * FROM (WITH a AS (SELECT 1 AS x) SELECT * FROM a) s",
    );
    // A live reference beside a shadowing one still counts.
    denied_with(
        &db,
        "qp_r2_shadow_app",
        "WITH a AS (SELECT * FROM qp_r2_shadow_secret) \
         SELECT * FROM a, (WITH a AS (SELECT 1 AS x) SELECT * FROM a) s",
        "permission denied for table qp_r2_shadow_secret",
    );
}

#[test]
fn certainly_folding_prunes_case_branches() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_case_app;
         CREATE FUNCTION qp_r2_case_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r2_case_f() FROM PUBLIC;",
    )]);
    allowed(
        &db,
        "qp_r2_case_app",
        "SELECT CASE WHEN 1 = 1 THEN 0 ELSE qp_r2_case_f() END",
    );
    denied_with(
        &db,
        "qp_r2_case_app",
        "SELECT CASE WHEN 1 = 2 THEN 0 ELSE qp_r2_case_f() END",
        "permission denied for function qp_r2_case_f",
    );
}

#[test]
fn plpgsql_declare_defaults_and_cursors_run_when_they_do() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_decl_app;
         CREATE TABLE qp_r2_decl_secret (x int);
         CREATE TABLE qp_r2_decl_1 (id int, x int);
         CREATE TABLE qp_r2_decl_2 (id int, x int);
         CREATE TABLE qp_r2_decl_3 (id int, x int);
         CREATE TABLE qp_r2_decl_4 (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON
             qp_r2_decl_1, qp_r2_decl_2, qp_r2_decl_3, qp_r2_decl_4
             TO qp_r2_decl_app;
         CREATE FUNCTION qp_r2_decl_cursor() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE c CURSOR FOR SELECT * FROM qp_r2_decl_secret;
         BEGIN RETURN NEW; END $$;
         CREATE FUNCTION qp_r2_decl_branch() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF TG_OP = 'DELETE' THEN
             DECLARE n int := (SELECT count(*) FROM qp_r2_decl_secret);
             BEGIN NULL; END;
           END IF; RETURN NEW; END $$;
         CREATE FUNCTION qp_r2_decl_entered() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE n int := (SELECT count(*) FROM qp_r2_decl_secret);
         BEGIN RETURN NEW; END $$;
         CREATE FUNCTION qp_r2_decl_open() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE c CURSOR FOR SELECT * FROM qp_r2_decl_secret;
         BEGIN OPEN c; RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_decl_1_trg BEFORE INSERT ON qp_r2_decl_1 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_decl_cursor();
         CREATE TRIGGER qp_r2_decl_2_trg BEFORE UPDATE ON qp_r2_decl_2 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_decl_branch();
         CREATE TRIGGER qp_r2_decl_3_trg BEFORE INSERT ON qp_r2_decl_3 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_decl_entered();
         CREATE TRIGGER qp_r2_decl_4_trg BEFORE INSERT ON qp_r2_decl_4 FOR EACH ROW
           EXECUTE FUNCTION qp_r2_decl_open();",
    )]);
    // A cursor's query isn't run when its block is entered.
    allowed(
        &db,
        "qp_r2_decl_app",
        "INSERT INTO qp_r2_decl_1 VALUES (1, 1)",
    );
    // A pruned branch's DECLARE default doesn't run.
    allowed(&db, "qp_r2_decl_app", "UPDATE qp_r2_decl_2 SET x = 1");
    // ... but an entered block's does, and so does an OPEN's cursor.
    denied_with(
        &db,
        "qp_r2_decl_app",
        "INSERT INTO qp_r2_decl_3 VALUES (1, 1)",
        "permission denied for table qp_r2_decl_secret",
    );
    denied_with(
        &db,
        "qp_r2_decl_app",
        "INSERT INTO qp_r2_decl_4 VALUES (1, 1)",
        "permission denied for table qp_r2_decl_secret",
    );
}

#[test]
fn simple_case_on_tg_op_prunes_like_an_if() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_scase_app;
         CREATE TABLE qp_r2_scase_secret (x int);
         CREATE TABLE qp_r2_scase_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_r2_scase_t TO qp_r2_scase_app;
         CREATE FUNCTION qp_r2_scase() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN CASE TG_OP WHEN 'DELETE' THEN
             INSERT INTO qp_r2_scase_secret VALUES (1);
           ELSE NULL; END CASE; RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_scase_trg BEFORE INSERT ON qp_r2_scase_t FOR EACH ROW
           EXECUTE FUNCTION qp_r2_scase();",
    )]);
    allowed(
        &db,
        "qp_r2_scase_app",
        "INSERT INTO qp_r2_scase_t VALUES (1, 2)",
    );
    // Firing the DELETE branch (a DELETE trigger) runs it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_scase2_app;
         CREATE TABLE qp_r2_scase2_secret (x int);
         CREATE TABLE qp_r2_scase2_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_r2_scase2_t TO qp_r2_scase2_app;
         CREATE FUNCTION qp_r2_scase2() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN CASE TG_OP WHEN 'DELETE' THEN
             INSERT INTO qp_r2_scase2_secret VALUES (1);
           ELSE NULL; END CASE; RETURN OLD; END $$;
         CREATE TRIGGER qp_r2_scase2_trg BEFORE DELETE ON qp_r2_scase2_t FOR EACH ROW
           EXECUTE FUNCTION qp_r2_scase2();",
    )]);
    denied_with(
        &db,
        "qp_r2_scase2_app",
        "DELETE FROM qp_r2_scase2_t",
        "permission denied for table qp_r2_scase2_secret",
    );
}

#[test]
fn nested_blocks_and_ifs_that_end_stop_the_enclosing_list() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_end_app;
         CREATE TABLE qp_r2_end_secret (x int);
         CREATE TABLE qp_r2_end_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_r2_end_t TO qp_r2_end_app;
         CREATE FUNCTION qp_r2_end_block() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN BEGIN RETURN NEW; END;
           INSERT INTO qp_r2_end_secret VALUES (1); RETURN NEW; END $$;
         CREATE FUNCTION qp_r2_end_if() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF TG_OP = 'INSERT' THEN RETURN NEW; ELSE RETURN NEW; END IF;
           INSERT INTO qp_r2_end_secret VALUES (1); RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_end_trg BEFORE INSERT ON qp_r2_end_t FOR EACH ROW
           EXECUTE FUNCTION qp_r2_end_block();",
    )]);
    allowed(
        &db,
        "qp_r2_end_app",
        "INSERT INTO qp_r2_end_t VALUES (1, 2)",
    );
    allowed(&db, "qp_r2_end_app", "SELECT qp_r2_end_if()");
    // An IF without ELSE may fall through: what follows still runs.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r2_end2_app;
         CREATE TABLE qp_r2_end2_secret (x int);
         CREATE TABLE qp_r2_end2_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_r2_end2_t TO qp_r2_end2_app;
         CREATE FUNCTION qp_r2_end2_if() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF TG_OP = 'INSERT' THEN RETURN NEW; END IF;
           INSERT INTO qp_r2_end2_secret VALUES (1); RETURN NEW; END $$;
         CREATE TRIGGER qp_r2_end2_trg BEFORE UPDATE ON qp_r2_end2_t FOR EACH ROW
           EXECUTE FUNCTION qp_r2_end2_if();",
    )]);
    denied_with(
        &db,
        "qp_r2_end2_app",
        "UPDATE qp_r2_end2_t SET x = 1",
        "permission denied for table qp_r2_end2_secret",
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Round 2 leftovers: statement/body checks (#3, #4, #6, #7, #10, #13–#16,
// #17, #18, #21–#23), role Describe (#30) and domain expression folding
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn update_only_skips_child_checks_and_indexes() {
    // ExecConstraints and ExecInsertIndexTuples run on the relations the
    // rows are actually modified in: `UPDATE ONLY parent` never reaches an
    // inheritance child's rows.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_only_app;
         CREATE FUNCTION qp_t7_only_check(int) RETURNS boolean LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t7_only_idx(int) RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN $1; END $$;
         CREATE TABLE qp_t7_only_parent (a int);
         CREATE TABLE qp_t7_only_child (CHECK (qp_t7_only_check(a)))
             INHERITS (qp_t7_only_parent);
         CREATE INDEX qp_t7_only_child_idx ON qp_t7_only_child ((qp_t7_only_idx(a)));
         INSERT INTO qp_t7_only_parent VALUES (0);
         INSERT INTO qp_t7_only_child VALUES (0);
         REVOKE EXECUTE ON FUNCTION qp_t7_only_check(int) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_t7_only_idx(int) FROM PUBLIC;
         GRANT UPDATE ON qp_t7_only_parent TO qp_t7_only_app;",
    )]);
    allowed(
        &db,
        "qp_t7_only_app",
        "UPDATE ONLY qp_t7_only_parent SET a = 1",
    );
    // Without ONLY the child's row is updated: its CHECK (or index
    // expression) runs.
    denied_with(
        &db,
        "qp_t7_only_app",
        "UPDATE qp_t7_only_parent SET a = 2",
        "permission denied for function qp_t7_only_check",
    );
}

#[test]
fn a_domain_typed_assignment_skips_the_domain_check() {
    // coerce_type returns a value whose type already is the domain's
    // unchanged: no CoerceToDomain, no constraint check.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_dom_app;
         CREATE FUNCTION qp_t7_dom_check(int) RETURNS boolean LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         CREATE DOMAIN qp_t7_dom_d AS int CHECK (qp_t7_dom_check(VALUE));
         CREATE TABLE qp_t7_dom_t (a qp_t7_dom_d);
         CREATE TABLE qp_t7_dom_src (a qp_t7_dom_d);
         INSERT INTO qp_t7_dom_t VALUES (1);
         INSERT INTO qp_t7_dom_src VALUES (2);
         REVOKE EXECUTE ON FUNCTION qp_t7_dom_check(int) FROM PUBLIC;
         GRANT SELECT, INSERT, UPDATE ON qp_t7_dom_t TO qp_t7_dom_app;
         GRANT SELECT ON qp_t7_dom_src TO qp_t7_dom_app;",
    )]);
    allowed(&db, "qp_t7_dom_app", "UPDATE qp_t7_dom_t SET a = a");
    allowed(
        &db,
        "qp_t7_dom_app",
        "INSERT INTO qp_t7_dom_t SELECT a FROM qp_t7_dom_src",
    );
    // A parameter whose type the assignment gives it is passed as a datum
    // of the domain: no coercion in the plan — but it arrives through the
    // domain's input / receive function, which evaluates its checks.
    denied_with(
        &db,
        "qp_t7_dom_app",
        "INSERT INTO qp_t7_dom_t VALUES ($1)",
        "permission denied for function qp_t7_dom_check",
    );
    // Any other expression type is coerced: the check runs.
    denied_with(
        &db,
        "qp_t7_dom_app",
        "INSERT INTO qp_t7_dom_t VALUES (1)",
        "permission denied for function qp_t7_dom_check",
    );
    denied_with(
        &db,
        "qp_t7_dom_app",
        "UPDATE qp_t7_dom_t SET a = 1",
        "permission denied for function qp_t7_dom_check",
    );
    denied_with(
        &db,
        "qp_t7_dom_app",
        "INSERT INTO qp_t7_dom_t VALUES (1::qp_t7_dom_d)",
        "permission denied for function qp_t7_dom_check",
    );
}

#[test]
fn a_user_function_named_nextval_is_not_the_sequence_function() {
    // PG classifies `nextval` / `currval` / `setval` by the resolved
    // function's OID (nextval_internal, do_setval), not by its name.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_seqfn_app;
         CREATE SEQUENCE qp_t7_seqfn_s;
         CREATE SCHEMA qp_t7_seqfn_ns;
         CREATE FUNCTION qp_t7_seqfn_ns.nextval(regclass) RETURNS bigint
             LANGUAGE plpgsql AS $$ BEGIN RETURN 42; END $$;
         CREATE FUNCTION qp_t7_seqfn_ns.setval(regclass, bigint) RETURNS bigint
             LANGUAGE plpgsql AS $$ BEGIN RETURN 43; END $$;
         SET search_path = qp_t7_seqfn_ns, public, pg_catalog;
         CREATE TABLE public.qp_t7_seqfn_user (
             a bigint DEFAULT nextval('qp_t7_seqfn_s'),
             b bigint DEFAULT setval('qp_t7_seqfn_s', 10)
         );
         CREATE TABLE public.qp_t7_seqfn_pg (
             a bigint DEFAULT pg_catalog.nextval('qp_t7_seqfn_s')
         );
         RESET search_path;
         GRANT INSERT ON qp_t7_seqfn_user, qp_t7_seqfn_pg TO qp_t7_seqfn_app;",
    )]);
    // The user functions need no sequence privilege; the pg_catalog one
    // does.
    allowed(
        &db,
        "qp_t7_seqfn_app",
        "INSERT INTO qp_t7_seqfn_user DEFAULT VALUES",
    );
    denied_with(
        &db,
        "qp_t7_seqfn_app",
        "INSERT INTO qp_t7_seqfn_pg DEFAULT VALUES",
        "permission denied for sequence qp_t7_seqfn_s",
    );
}

#[test]
fn partitioned_writes_evaluate_the_partitions_generation_expression() {
    // ExecInsert routes the row before ExecInitGenerated: the generation
    // expression evaluated is the physical partition's own.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_gen_app;
         CREATE FUNCTION qp_t7_gen_parent(int) RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN $1; END $$;
         CREATE TABLE qp_t7_gen_p (
             k int, a int, g int GENERATED ALWAYS AS (qp_t7_gen_parent(a)) STORED
         ) PARTITION BY LIST (k);
         CREATE TABLE qp_t7_gen_p1 PARTITION OF qp_t7_gen_p
             (g GENERATED ALWAYS AS (a * 2) STORED) FOR VALUES IN (1);
         REVOKE EXECUTE ON FUNCTION qp_t7_gen_parent(int) FROM PUBLIC;
         GRANT INSERT, UPDATE ON qp_t7_gen_p TO qp_t7_gen_app;",
    )]);
    // The partition overrides the parent's expression: the parent's function
    // never runs.
    allowed(
        &db,
        "qp_t7_gen_app",
        "INSERT INTO qp_t7_gen_p (k, a) VALUES (1, 5)",
    );

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_gen2_app;
         CREATE FUNCTION qp_t7_gen2_part(int) RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN $1; END $$;
         CREATE TABLE qp_t7_gen2_p (
             k int, a int, g int GENERATED ALWAYS AS (a) STORED
         ) PARTITION BY LIST (k);
         CREATE TABLE qp_t7_gen2_p1 PARTITION OF qp_t7_gen2_p
             (g GENERATED ALWAYS AS (qp_t7_gen2_part(a)) STORED) FOR VALUES IN (1);
         INSERT INTO qp_t7_gen2_p (k, a) VALUES (1, 5);
         REVOKE EXECUTE ON FUNCTION qp_t7_gen2_part(int) FROM PUBLIC;
         GRANT INSERT, UPDATE ON qp_t7_gen2_p TO qp_t7_gen2_app;",
    )]);
    denied_with(
        &db,
        "qp_t7_gen2_app",
        "INSERT INTO qp_t7_gen2_p (k, a) VALUES (1, 6)",
        "permission denied for function qp_t7_gen2_part",
    );
    // The update of the parent's row recomputes the partition's expression.
    denied_with(
        &db,
        "qp_t7_gen2_app",
        "UPDATE qp_t7_gen2_p SET a = 6",
        "permission denied for function qp_t7_gen2_part",
    );

    // A partition without an expression of its own evaluates the parent's
    // (copied into its `pg_attrdef`).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_gen3_app;
         CREATE FUNCTION qp_t7_gen3_gen(int) RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN $1; END $$;
         CREATE TABLE qp_t7_gen3_p (
             k int, a int, g int GENERATED ALWAYS AS (qp_t7_gen3_gen(a)) STORED
         ) PARTITION BY LIST (k);
         CREATE TABLE qp_t7_gen3_p1 PARTITION OF qp_t7_gen3_p FOR VALUES IN (1);
         REVOKE EXECUTE ON FUNCTION qp_t7_gen3_gen(int) FROM PUBLIC;
         GRANT INSERT ON qp_t7_gen3_p TO qp_t7_gen3_app;",
    )]);
    denied_with(
        &db,
        "qp_t7_gen3_app",
        "INSERT INTO qp_t7_gen3_p (k, a) VALUES (1, 5)",
        "permission denied for function qp_t7_gen3_gen",
    );
}

#[test]
fn a_view_over_only_does_not_reach_child_triggers() {
    // rewriteTargetView keeps the view base RTE's inheritance flag: the
    // write it hands to the base relation reaches no inheritance child.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_vonly_app;
         CREATE TABLE qp_t7_vonly_parent (a int);
         CREATE TABLE qp_t7_vonly_child () INHERITS (qp_t7_vonly_parent);
         CREATE TABLE qp_t7_vonly_audit (a int);
         INSERT INTO qp_t7_vonly_parent VALUES (0);
         INSERT INTO qp_t7_vonly_child VALUES (0);
         CREATE FUNCTION qp_t7_vonly_tr() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN INSERT INTO qp_t7_vonly_audit VALUES (1); RETURN NEW; END $$;
         CREATE TRIGGER qp_t7_vonly_trg BEFORE UPDATE ON qp_t7_vonly_child
             FOR EACH ROW EXECUTE FUNCTION qp_t7_vonly_tr();
         CREATE VIEW qp_t7_vonly_only AS SELECT a FROM ONLY qp_t7_vonly_parent;
         CREATE VIEW qp_t7_vonly_all AS SELECT a FROM qp_t7_vonly_parent;
         GRANT UPDATE ON qp_t7_vonly_only, qp_t7_vonly_all TO qp_t7_vonly_app;",
    )]);
    allowed(&db, "qp_t7_vonly_app", "UPDATE qp_t7_vonly_only SET a = 1");
    denied_with(
        &db,
        "qp_t7_vonly_app",
        "UPDATE qp_t7_vonly_all SET a = 2",
        "permission denied for table qp_t7_vonly_audit",
    );
}

#[test]
fn assigning_a_tg_variable_makes_it_unknown() {
    // TG_* promise variables may be assigned; PL/pgSQL explicitly does not
    // keep their initial values (assign_simple_var).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_tg_app;
         CREATE TABLE qp_t7_tg_t (a int);
         CREATE TABLE qp_t7_tg_audit (a int);
         CREATE FUNCTION qp_t7_tg_tr() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           TG_OP := 'DELETE';
           IF TG_OP = 'DELETE' THEN INSERT INTO qp_t7_tg_audit VALUES (1); END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_t7_tg_trg BEFORE INSERT ON qp_t7_tg_t
             FOR EACH ROW EXECUTE FUNCTION qp_t7_tg_tr();
         GRANT INSERT ON qp_t7_tg_t TO qp_t7_tg_app;",
    )]);
    denied_with(
        &db,
        "qp_t7_tg_app",
        "INSERT INTO qp_t7_tg_t VALUES (1)",
        "permission denied for table qp_t7_tg_audit",
    );
}

#[test]
fn an_unplanned_cte_still_checks_schema_usage() {
    // LookupExplicitNamespace runs at parse analysis, before SS_process_ctes
    // discards the unreferenced CTE.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_cte_app;
         CREATE SCHEMA qp_t7_cte_locked;
         CREATE TABLE qp_t7_cte_locked.t (a int);
         GRANT SELECT ON qp_t7_cte_locked.t TO qp_t7_cte_app;",
    )]);
    denied_with(
        &db,
        "qp_t7_cte_app",
        "WITH unused AS (SELECT * FROM qp_t7_cte_locked.t) SELECT 1",
        "permission denied for schema qp_t7_cte_locked",
    );
}

#[test]
fn an_explicit_default_through_a_view_gets_the_base_default() {
    // rewriteTargetListIU removes the explicit DEFAULT with no default of
    // the view's own, so the base column's default applies on the recursive
    // rewrite.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_vdef_app;
         CREATE TABLE qp_t7_vdef_t (id serial);
         CREATE VIEW qp_t7_vdef_v AS SELECT id FROM qp_t7_vdef_t;
         GRANT INSERT ON qp_t7_vdef_v TO qp_t7_vdef_app;",
    )]);
    denied_with(
        &db,
        "qp_t7_vdef_app",
        "INSERT INTO qp_t7_vdef_v (id) VALUES (DEFAULT)",
        "permission denied for sequence qp_t7_vdef_t_id_seq",
    );

    // A view column with its own default uses that instead of the base's.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_vdef2_app;
         CREATE TABLE qp_t7_vdef2_t (id serial);
         CREATE VIEW qp_t7_vdef2_v AS SELECT id FROM qp_t7_vdef2_t;
         ALTER VIEW qp_t7_vdef2_v ALTER COLUMN id SET DEFAULT 7;
         GRANT INSERT ON qp_t7_vdef2_v TO qp_t7_vdef2_app;",
    )]);
    allowed(
        &db,
        "qp_t7_vdef2_app",
        "INSERT INTO qp_t7_vdef2_v (id) VALUES (DEFAULT)",
    );
}

#[test]
fn instead_of_triggers_keep_the_view_query() {
    // ApplyRetrieveRule expands the view to find the rows the trigger is
    // handed, whatever the INSTEAD OF trigger does with them.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_instead_app;
         CREATE TABLE qp_t7_instead_t (a int);
         INSERT INTO qp_t7_instead_t VALUES (1);
         CREATE FUNCTION qp_t7_instead_gate(int) RETURNS boolean LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         CREATE VIEW qp_t7_instead_v AS
             SELECT a FROM qp_t7_instead_t WHERE qp_t7_instead_gate(a);
         CREATE FUNCTION qp_t7_instead_tr() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN RETURN NEW; END $$;
         CREATE TRIGGER qp_t7_instead_upd INSTEAD OF UPDATE ON qp_t7_instead_v
             FOR EACH ROW EXECUTE FUNCTION qp_t7_instead_tr();
         CREATE TRIGGER qp_t7_instead_del INSTEAD OF DELETE ON qp_t7_instead_v
             FOR EACH ROW EXECUTE FUNCTION qp_t7_instead_tr();
         REVOKE EXECUTE ON FUNCTION qp_t7_instead_gate(int) FROM PUBLIC;
         GRANT UPDATE, DELETE ON qp_t7_instead_v TO qp_t7_instead_app;",
    )]);
    denied_with(
        &db,
        "qp_t7_instead_app",
        "UPDATE qp_t7_instead_v SET a = 2",
        "permission denied for function qp_t7_instead_gate",
    );
    denied_with(
        &db,
        "qp_t7_instead_app",
        "DELETE FROM qp_t7_instead_v",
        "permission denied for function qp_t7_instead_gate",
    );
}

#[test]
fn requirements_on_one_sequence_are_all_checked() {
    // nextval needs USAGE or UPDATE, setval UPDATE: the role must satisfy
    // each requirement, not just the first one recorded.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_seq_app;
         CREATE SEQUENCE qp_t7_seq_s;
         CREATE TABLE qp_t7_seq_t (
             a bigint DEFAULT nextval('qp_t7_seq_s'),
             b bigint DEFAULT setval('qp_t7_seq_s', 10)
         );
         CREATE TABLE qp_t7_seq_t2 (
             b bigint DEFAULT setval('qp_t7_seq_s', 10),
             a bigint DEFAULT nextval('qp_t7_seq_s')
         );
         GRANT INSERT ON qp_t7_seq_t, qp_t7_seq_t2 TO qp_t7_seq_app;
         GRANT USAGE ON SEQUENCE qp_t7_seq_s TO qp_t7_seq_app;",
    )]);
    for table in ["qp_t7_seq_t", "qp_t7_seq_t2"] {
        denied_with(
            &db,
            "qp_t7_seq_app",
            &format!("INSERT INTO {table} DEFAULT VALUES"),
            "permission denied for sequence qp_t7_seq_s",
        );
    }

    // UPDATE satisfies both calls.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_seq2_app;
         CREATE SEQUENCE qp_t7_seq2_s;
         CREATE TABLE qp_t7_seq2_t (
             a bigint DEFAULT nextval('qp_t7_seq2_s'),
             b bigint DEFAULT setval('qp_t7_seq2_s', 10)
         );
         GRANT INSERT ON qp_t7_seq2_t TO qp_t7_seq2_app;
         GRANT UPDATE ON SEQUENCE qp_t7_seq2_s TO qp_t7_seq2_app;",
    )]);
    allowed(
        &db,
        "qp_t7_seq2_app",
        "INSERT INTO qp_t7_seq2_t DEFAULT VALUES",
    );
}

#[test]
fn a_domain_default_runs_its_sequence_functions() {
    // build_column_default falls back to the domain's DEFAULT, whose
    // nextval runs with the statement.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_dseq_app;
         CREATE SEQUENCE qp_t7_dseq_s;
         CREATE DOMAIN qp_t7_dseq_d AS bigint DEFAULT nextval('qp_t7_dseq_s');
         CREATE TABLE qp_t7_dseq_t (a qp_t7_dseq_d);
         GRANT INSERT ON qp_t7_dseq_t TO qp_t7_dseq_app;",
    )]);
    denied_with(
        &db,
        "qp_t7_dseq_app",
        "INSERT INTO qp_t7_dseq_t DEFAULT VALUES",
        "permission denied for sequence qp_t7_dseq_s",
    );
}

#[test]
fn dynamic_for_loops_check_their_static_body() {
    // exec_stmt_dynfors executes its static loop body; only the query
    // string it runs is unknown.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_dfor_app;
         CREATE TABLE qp_t7_dfor_secret (a int);
         CREATE FUNCTION qp_t7_dfor_f() RETURNS int LANGUAGE plpgsql AS $$
         DECLARE r record;
         BEGIN
           FOR r IN EXECUTE 'SELECT 1' LOOP
             PERFORM a FROM qp_t7_dfor_secret;
           END LOOP;
           RETURN 1;
         END $$;",
    )]);
    denied_with(
        &db,
        "qp_t7_dfor_app",
        "SELECT qp_t7_dfor_f()",
        "permission denied for table qp_t7_dfor_secret",
    );
}

#[test]
fn open_checks_the_query_it_opens() {
    // exec_stmt_open plans and starts the static query (SPI_cursor_open),
    // and evaluates the expression of a dynamic one.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_open_app;
         CREATE TABLE qp_t7_open_secret (a int);
         CREATE FUNCTION qp_t7_open_static() RETURNS int LANGUAGE plpgsql AS $$
         DECLARE c refcursor;
         BEGIN
           OPEN c FOR SELECT a FROM qp_t7_open_secret;
           CLOSE c;
           RETURN 1;
         END $$;
         CREATE FUNCTION qp_t7_open_text() RETURNS text LANGUAGE plpgsql
             AS $$ BEGIN RETURN 'SELECT 1'; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t7_open_text() FROM PUBLIC;
         CREATE FUNCTION qp_t7_open_dynamic() RETURNS int LANGUAGE plpgsql AS $$
         DECLARE c refcursor;
         BEGIN
           OPEN c FOR EXECUTE qp_t7_open_text();
           CLOSE c;
           RETURN 1;
         END $$;",
    )]);
    denied_with(
        &db,
        "qp_t7_open_app",
        "SELECT qp_t7_open_static()",
        "permission denied for table qp_t7_open_secret",
    );
    // The dynamic OPEN checks the expression producing the query string,
    // never the (unknown) SQL it holds.
    denied_with(
        &db,
        "qp_t7_open_app",
        "SELECT qp_t7_open_dynamic()",
        "permission denied for function qp_t7_open_text",
    );
}

#[test]
fn execute_checks_the_expression_that_produces_its_sql() {
    // exec_stmt_dynexecute evaluates the query-string expression before it
    // runs the (unknown) dynamic SQL.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_dyn_app;
         CREATE FUNCTION qp_t7_dyn_text() RETURNS text LANGUAGE plpgsql
             AS $$ BEGIN RETURN 'SELECT 1'; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t7_dyn_text() FROM PUBLIC;
         CREATE FUNCTION qp_t7_dyn_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN EXECUTE qp_t7_dyn_text(); RETURN 1; END $$;",
    )]);
    denied_with(
        &db,
        "qp_t7_dyn_app",
        "SELECT qp_t7_dyn_f()",
        "permission denied for function qp_t7_dyn_text",
    );
}

#[test]
fn call_after_a_leading_comment_is_still_run_by_the_oracle() {
    // The oracle recognizes CALL from the parse tree, not from the text's
    // first four characters (comments and whitespace may precede it).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_call_app;
         CREATE PROCEDURE qp_t7_call_p() LANGUAGE plpgsql AS $$ BEGIN NULL; END $$;
         REVOKE EXECUTE ON PROCEDURE qp_t7_call_p() FROM PUBLIC;",
    )]);
    denied_with(
        &db,
        "qp_t7_call_app",
        " /* a comment */ CALL qp_t7_call_p()",
        "permission denied for procedure qp_t7_call_p",
    );
}

#[test]
fn a_role_analysis_resolves_names_through_its_login_search_path() {
    // recomputeNamespacePath: a fresh session of the role resolves an
    // unqualified name along its own `ALTER ROLE ... SET search_path`.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_path_app;
         CREATE SCHEMA qp_t7_path_s;
         CREATE TABLE qp_t7_path_t (x int);
         CREATE TABLE qp_t7_path_s.qp_t7_path_t (x text);
         GRANT USAGE ON SCHEMA qp_t7_path_s TO qp_t7_path_app;
         GRANT SELECT ON qp_t7_path_t, qp_t7_path_s.qp_t7_path_t TO qp_t7_path_app;
         ALTER ROLE qp_t7_path_app SET search_path = qp_t7_path_s, public;",
    )]);
    let analyzed = db
        .analyze_as("SELECT x FROM qp_t7_path_t", "qp_t7_path_app")
        .expect("the role may read the table");
    assert_eq!(analyzed.columns.len(), 1);
    assert!(
        matches!(&analyzed.columns[0].pg_type, Type::Basic { name, .. } if name == "text"),
        "the role's search path resolves the text column: {:?}",
        analyzed.columns[0].pg_type
    );

    // The oracle compares this analysis with PG's Describe of the same
    // statement prepared as the role: a wrong column name or type is
    // caught.
    #[cfg(feature = "pg_sanity")]
    {
        let mut wrong = analyzed.clone();
        wrong.columns[0].name = "wrong_name".to_owned();
        let divergence = db
            .pg_sanity_check_role_analysis("SELECT x FROM qp_t7_path_t", "qp_t7_path_app", &wrong)
            .expect("the oracle sees the wrong column name");
        assert_eq!(
            divergence.kind,
            typedpg_analyzer::DivergenceKind::ColumnName
        );
    }
}

#[test]
fn domain_check_and_default_expressions_are_folded() {
    // A domain's CHECK / DEFAULT expressions are cooked and then run
    // through expression_planner like a table's: code the planner folds away
    // never runs, and needs no EXECUTE.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_dfold_app;
         CREATE FUNCTION qp_t7_dfold_check(int) RETURNS boolean LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t7_dfold_default() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 7; END $$;
         CREATE DOMAIN qp_t7_dfold_d AS int
             CHECK (CASE WHEN true THEN true ELSE qp_t7_dfold_check(VALUE) END);
         CREATE DOMAIN qp_t7_dfold_dd AS int
             DEFAULT (CASE WHEN false THEN qp_t7_dfold_default() ELSE 1 END);
         CREATE TABLE qp_t7_dfold_t (a qp_t7_dfold_d);
         CREATE TABLE qp_t7_dfold_td (a qp_t7_dfold_dd);
         REVOKE EXECUTE ON FUNCTION qp_t7_dfold_check(int) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_t7_dfold_default() FROM PUBLIC;
         GRANT INSERT ON qp_t7_dfold_t, qp_t7_dfold_td TO qp_t7_dfold_app;",
    )]);
    allowed(
        &db,
        "qp_t7_dfold_app",
        "INSERT INTO qp_t7_dfold_t VALUES (1)",
    );
    allowed(
        &db,
        "qp_t7_dfold_app",
        "INSERT INTO qp_t7_dfold_td DEFAULT VALUES",
    );
}

#[test]
fn a_table_revoke_uses_each_columns_own_grantor() {
    // Earlier-round finding (already fixed): ExecGrant_Attribute selects the
    // grantor from the table's and the column's combined ACLs, so a member's
    // table revoke leaves a grant another grantor made intact.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t7_grantor_g;
         CREATE ROLE qp_t7_grantor_member;
         CREATE ROLE qp_t7_grantor_app;
         CREATE TABLE qp_t7_grantor_t (a int);
         GRANT SELECT ON qp_t7_grantor_t TO qp_t7_grantor_g WITH GRANT OPTION;
         GRANT qp_t7_grantor_g TO qp_t7_grantor_member;
         SET ROLE qp_t7_grantor_g;
         GRANT SELECT (a) ON qp_t7_grantor_t TO qp_t7_grantor_app;
         RESET ROLE;
         GRANT SELECT (a) ON qp_t7_grantor_t TO qp_t7_grantor_member WITH GRANT OPTION;
         SET ROLE qp_t7_grantor_member;
         REVOKE SELECT ON qp_t7_grantor_t FROM qp_t7_grantor_app;
         RESET ROLE;",
    )]);
    let table = crate::common::class_oid(&db, Some("public"), "qp_t7_grantor_t");
    assert!(
        db.has_column_privilege(
            "qp_t7_grantor_app",
            table,
            1,
            typedpg_analyzer::Privilege::Select
        ),
        "g's column grant must survive member's table revoke"
    );
    allowed(&db, "qp_t7_grantor_app", "SELECT a FROM qp_t7_grantor_t");
}

// ──────────────────────────────────────────────────────────────────────────────
// Third-round findings: stored queries, planner folding and body detail
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn a_views_stored_query_needs_no_schema_usage() {
    // PG stores a view's query bound to OIDs: reading the view expands it
    // without `LookupExplicitNamespace`, so the schema its relations and
    // routines live in needs no USAGE (PostgREST's views over a private
    // schema). A `SECURITY INVOKER` / `SECURITY BARRIER` view is the same:
    // only the roles the query reads as change, not the name lookups.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_vs_app;
         CREATE SCHEMA qp_r3_vs_private;
         CREATE TABLE qp_r3_vs_private.t (id int, v text);
         CREATE TABLE qp_r3_vs_pub (id int, v text);
         CREATE FUNCTION qp_r3_vs_private.f(int) RETURNS int LANGUAGE sql IMMUTABLE
             AS 'SELECT $1';
         CREATE VIEW qp_r3_vs_plain AS SELECT id FROM qp_r3_vs_private.t;
         CREATE VIEW qp_r3_vs_invoker WITH (security_invoker = true)
             AS SELECT id FROM qp_r3_vs_private.t;
         CREATE VIEW qp_r3_vs_barrier WITH (security_barrier = true)
             AS SELECT id FROM qp_r3_vs_private.t WHERE v IS NOT NULL;
         CREATE VIEW qp_r3_vs_fn AS SELECT qp_r3_vs_private.f(id) AS x FROM qp_r3_vs_pub;
         CREATE VIEW qp_r3_vs_upd AS SELECT id, v FROM qp_r3_vs_private.t;
         GRANT SELECT, UPDATE ON
             qp_r3_vs_plain, qp_r3_vs_invoker, qp_r3_vs_barrier, qp_r3_vs_fn, qp_r3_vs_upd
             TO qp_r3_vs_app;
         -- A `security_invoker` view reads its relations as the caller.
         GRANT SELECT (id), UPDATE ON qp_r3_vs_private.t TO qp_r3_vs_app;",
    )]);
    for sql in [
        "SELECT id FROM qp_r3_vs_plain",
        "SELECT id FROM qp_r3_vs_invoker",
        "SELECT id FROM qp_r3_vs_barrier",
        "SELECT x FROM qp_r3_vs_fn",
        "UPDATE qp_r3_vs_upd SET v = 'x' WHERE id = 1",
    ] {
        allowed(&db, "qp_r3_vs_app", sql);
    }
    // A `SECURITY INVOKER` function body given as a string *is* parsed at
    // run time: its schema-qualified names need USAGE.
    denied_with(
        &db,
        "qp_r3_vs_app",
        "SELECT qp_r3_vs_private.f(1)",
        "permission denied for schema qp_r3_vs_private",
    );
}

#[test]
fn unused_view_and_subquery_outputs_are_not_evaluated() {
    // `pull_up_simple_subquery` / `remove_unused_subquery_outputs` drop an
    // output nothing reads: its expression's functions, sublinks and
    // relations never run. A volatile expression — or a view the planner
    // plans on its own with a plain DISTINCT — is kept.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_out_app;
         CREATE TABLE qp_r3_out_t (id int, v text);
         CREATE TABLE qp_r3_out_likes (post int);
         CREATE FUNCTION qp_r3_out_secret(int) RETURNS int LANGUAGE plpgsql STABLE
             AS $$ BEGIN RETURN $1; END $$;
         CREATE FUNCTION qp_r3_out_vol(int) RETURNS int LANGUAGE plpgsql VOLATILE
             AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r3_out_secret(int) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_r3_out_vol(int) FROM PUBLIC;
         CREATE VIEW qp_r3_out_plain AS
             SELECT id, qp_r3_out_secret(id) AS masked, v FROM qp_r3_out_t;
         CREATE VIEW qp_r3_out_volatile AS
             SELECT id, qp_r3_out_vol(id) AS masked FROM qp_r3_out_t;
         CREATE VIEW qp_r3_out_distinct AS
             SELECT DISTINCT id, qp_r3_out_secret(id) AS masked FROM qp_r3_out_t;
         CREATE VIEW qp_r3_out_sublink AS
             SELECT id, (SELECT count(*) FROM qp_r3_out_likes l WHERE l.post = t.id) AS likes
             FROM qp_r3_out_t t;
         GRANT SELECT ON qp_r3_out_plain, qp_r3_out_volatile, qp_r3_out_distinct,
             qp_r3_out_sublink, qp_r3_out_t TO qp_r3_out_app;",
    )]);
    // The unused output's function is never initialized.
    allowed(&db, "qp_r3_out_app", "SELECT id FROM qp_r3_out_plain");
    allowed(
        &db,
        "qp_r3_out_app",
        "SELECT id FROM (SELECT id, qp_r3_out_secret(id) AS masked FROM qp_r3_out_t) x",
    );
    allowed(
        &db,
        "qp_r3_out_app",
        "WITH c AS (SELECT id, qp_r3_out_secret(id) AS masked FROM qp_r3_out_t)
         SELECT id FROM c",
    );
    // A sublink relation in an unused output isn't planned.
    allowed(&db, "qp_r3_out_app", "SELECT id FROM qp_r3_out_sublink");
    // A whole row, `*` and an expression read them all.
    denied_with(
        &db,
        "qp_r3_out_app",
        "SELECT * FROM qp_r3_out_plain",
        "permission denied for function qp_r3_out_secret",
    );
    denied_with(
        &db,
        "qp_r3_out_app",
        "SELECT qp_r3_out_plain FROM qp_r3_out_plain",
        "permission denied for function qp_r3_out_secret",
    );
    // Volatile expressions and DISTINCT outputs are evaluated anyway.
    denied_with(
        &db,
        "qp_r3_out_app",
        "SELECT id FROM qp_r3_out_volatile",
        "permission denied for function qp_r3_out_vol",
    );
    denied_with(
        &db,
        "qp_r3_out_app",
        "SELECT id FROM qp_r3_out_distinct",
        "permission denied for function qp_r3_out_secret",
    );
    // A materialized CTE is planned and run whole.
    denied_with(
        &db,
        "qp_r3_out_app",
        "WITH c AS MATERIALIZED
             (SELECT id, qp_r3_out_secret(id) AS masked FROM qp_r3_out_t)
         SELECT id FROM c",
        "permission denied for function qp_r3_out_secret",
    );
}

#[test]
fn explain_without_analyze_only_starts_the_executor() {
    // ExplainOnePlan calls ExecutorStart but never ExecutorRun: the plan's
    // relations and functions are checked (ExecCheckPermissions,
    // ExecInitExpr), nothing evaluates a row — no `nextval` from a default,
    // no trigger, no routine body. EXPLAIN ANALYZE runs the statement.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_ex_app;
         CREATE SEQUENCE qp_r3_ex_s;
         CREATE TABLE qp_r3_ex_t (id int, a bigint DEFAULT nextval('qp_r3_ex_s'));
         CREATE TABLE qp_r3_ex_u (id int);
         CREATE TABLE qp_r3_ex_audit (a int);
         CREATE TABLE qp_r3_ex_secret (a int);
         CREATE FUNCTION qp_r3_ex_fn() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 7; END $$;
         CREATE FUNCTION qp_r3_ex_tr() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN INSERT INTO qp_r3_ex_audit VALUES (1); RETURN NEW; END $$;
         CREATE TRIGGER qp_r3_ex_trg BEFORE INSERT ON qp_r3_ex_u
             FOR EACH ROW EXECUTE FUNCTION qp_r3_ex_tr();
         REVOKE EXECUTE ON FUNCTION qp_r3_ex_fn() FROM PUBLIC;
         GRANT INSERT ON qp_r3_ex_t, qp_r3_ex_u TO qp_r3_ex_app;
         GRANT SELECT, INSERT ON qp_r3_ex_secret TO qp_r3_ex_app;",
    )]);
    // No sequence privilege: the default's `nextval` never runs.
    allowed(
        &db,
        "qp_r3_ex_app",
        "EXPLAIN INSERT INTO qp_r3_ex_t (id) VALUES (1)",
    );
    // No trigger body: no row is inserted.
    allowed(
        &db,
        "qp_r3_ex_app",
        "EXPLAIN INSERT INTO qp_r3_ex_u VALUES (1)",
    );
    // The plan's own expressions are initialized: EXECUTE is checked.
    denied_with(
        &db,
        "qp_r3_ex_app",
        "EXPLAIN SELECT qp_r3_ex_fn()",
        "permission denied for function qp_r3_ex_fn",
    );
    // EXPLAIN ANALYZE executes: all of it runs.
    denied_with(
        &db,
        "qp_r3_ex_app",
        "EXPLAIN ANALYZE INSERT INTO qp_r3_ex_t (id) VALUES (1)",
        "permission denied for sequence qp_r3_ex_s",
    );
    denied_with(
        &db,
        "qp_r3_ex_app",
        "EXPLAIN ANALYZE INSERT INTO qp_r3_ex_u VALUES (1)",
        "permission denied for table qp_r3_ex_audit",
    );
}

#[test]
fn plpgsql_declaration_types_need_schema_usage() {
    // The function's body is compiled at its first call: a declared type,
    // `%TYPE` or `%ROWTYPE` naming another schema runs
    // `LookupExplicitNamespace` as the role calling it. Types in the
    // function's own signature were resolved at CREATE FUNCTION.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_decl_app;
         CREATE ROLE qp_r3_decl_other;
         CREATE SCHEMA qp_r3_decl_private;
         CREATE TABLE qp_r3_decl_private.users (id int);
         CREATE TYPE qp_r3_decl_private.status AS ENUM ('a');
         CREATE FUNCTION qp_r3_decl_f() RETURNS int LANGUAGE plpgsql AS $$
         DECLARE r qp_r3_decl_private.users%ROWTYPE;
                 x qp_r3_decl_private.users.id%TYPE;
                 s qp_r3_decl_private.status;
         BEGIN RETURN 1; END $$;
         CREATE FUNCTION qp_r3_decl_g(a qp_r3_decl_private.status)
             RETURNS qp_r3_decl_private.status LANGUAGE plpgsql
             AS $$ BEGIN RETURN a; END $$;
         GRANT EXECUTE ON FUNCTION qp_r3_decl_f(), qp_r3_decl_g(qp_r3_decl_private.status)
             TO qp_r3_decl_app;
         GRANT EXECUTE ON FUNCTION qp_r3_decl_f(), qp_r3_decl_g(qp_r3_decl_private.status)
             TO qp_r3_decl_other;
         GRANT USAGE ON SCHEMA qp_r3_decl_private TO qp_r3_decl_other;",
    )]);
    denied_with(
        &db,
        "qp_r3_decl_app",
        "SELECT qp_r3_decl_f()",
        "permission denied for schema qp_r3_decl_private",
    );
    // The signature's types need no USAGE to call it.
    allowed(&db, "qp_r3_decl_app", "SELECT qp_r3_decl_g(NULL)");
    allowed(&db, "qp_r3_decl_other", "SELECT qp_r3_decl_f()");
}

#[test]
fn a_strict_call_with_a_null_constant_needs_no_execute() {
    // evaluate_function replaces a strict call with any NULL constant
    // argument by NULL before the executor initializes it: neither the
    // routine nor its arguments run.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_strict_app;
         CREATE FUNCTION qp_r3_strict_secret(int) RETURNS int LANGUAGE plpgsql STRICT
             AS $$ BEGIN RETURN $1; END $$;
         CREATE FUNCTION qp_r3_strict_secret(int, int) RETURNS int LANGUAGE plpgsql STRICT
             AS $$ BEGIN RETURN $1; END $$;
         CREATE FUNCTION qp_r3_strict_arg() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r3_strict_secret(int) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_r3_strict_secret(int, int) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION qp_r3_strict_arg() FROM PUBLIC;",
    )]);
    allowed(
        &db,
        "qp_r3_strict_app",
        "SELECT qp_r3_strict_secret(NULL::int)",
    );
    allowed(&db, "qp_r3_strict_app", "SELECT qp_r3_strict_secret(NULL)");
    // The other arguments are dropped too.
    allowed(
        &db,
        "qp_r3_strict_app",
        "SELECT qp_r3_strict_secret(NULL::int, qp_r3_strict_arg())",
    );
    allowed(
        &db,
        "qp_r3_strict_app",
        "SELECT qp_r3_strict_secret(qp_r3_strict_arg(), NULL::int)",
    );
    // A non-NULL argument still runs the call.
    denied_with(
        &db,
        "qp_r3_strict_app",
        "SELECT qp_r3_strict_secret(1)",
        "permission denied for function qp_r3_strict_secret",
    );
}

#[test]
fn certain_casts_recognize_typed_nulls_and_booleans() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_cast_app;
         CREATE FUNCTION qp_r3_cast_secret() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 7; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r3_cast_secret() FROM PUBLIC;",
    )]);
    // coerce_type makes an untyped NULL a NULL Const of the target type.
    allowed(
        &db,
        "qp_r3_cast_app",
        "SELECT CASE WHEN NULL::boolean THEN qp_r3_cast_secret() ELSE 1 END",
    );
    // `'false'::boolean` is the false constant at parse time.
    allowed(
        &db,
        "qp_r3_cast_app",
        "SELECT CASE WHEN 'false'::boolean THEN qp_r3_cast_secret() ELSE 1 END",
    );
    // `'true'::boolean` takes the THEN branch.
    denied_with(
        &db,
        "qp_r3_cast_app",
        "SELECT CASE WHEN 'true'::boolean THEN qp_r3_cast_secret() ELSE 1 END",
        "permission denied for function qp_r3_cast_secret",
    );
}

#[test]
fn plpgsql_conditions_use_the_planners_certainty() {
    // PL/pgSQL plans `IF <expr>` as `SELECT <expr>`: the planner folds
    // `1 = 0` and a typed NULL before the branch runs.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_if_app;
         CREATE TABLE qp_r3_if_secret (id int);
         CREATE FUNCTION qp_r3_if_f() RETURNS int LANGUAGE plpgsql AS $$
         BEGIN
           IF 1 = 0 THEN PERFORM id FROM qp_r3_if_secret; END IF;
           IF 1 > 2 THEN PERFORM id FROM qp_r3_if_secret; END IF;
           WHILE 2 = 3 LOOP PERFORM id FROM qp_r3_if_secret; END LOOP;
           RETURN 1;
         END $$;
         GRANT EXECUTE ON FUNCTION qp_r3_if_f() TO qp_r3_if_app;",
    )]);
    allowed(&db, "qp_r3_if_app", "SELECT qp_r3_if_f()");
}

#[test]
fn tg_comparisons_respect_varchar_truncation() {
    // `'INSERTx'::varchar(6)` truncates to `INSERT` before the comparison,
    // so the trigger's body doesn't run.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_vc_app;
         CREATE TABLE qp_r3_vc_t (a int);
         CREATE TABLE qp_r3_vc_audit (a int);
         CREATE FUNCTION qp_r3_vc_ne() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF TG_OP <> 'INSERTx'::varchar(6) THEN
             INSERT INTO qp_r3_vc_audit VALUES (1);
           END IF;
           RETURN NEW;
         END $$;
         CREATE FUNCTION qp_r3_vc_eq() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF TG_OP = 'INSERTx'::varchar(6) THEN
             INSERT INTO qp_r3_vc_audit VALUES (1);
           END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_r3_vc_ne_trg BEFORE INSERT ON qp_r3_vc_t
             FOR EACH ROW EXECUTE FUNCTION qp_r3_vc_ne();
         CREATE TRIGGER qp_r3_vc_eq_trg AFTER INSERT ON qp_r3_vc_t
             FOR EACH ROW EXECUTE FUNCTION qp_r3_vc_eq();
         CREATE TABLE qp_r3_vc_t2 (a int);
         CREATE TRIGGER qp_r3_vc_ne_trg2 BEFORE INSERT ON qp_r3_vc_t2
             FOR EACH ROW EXECUTE FUNCTION qp_r3_vc_ne();
         GRANT INSERT ON qp_r3_vc_t, qp_r3_vc_t2 TO qp_r3_vc_app;",
    )]);
    // Both triggers fire for the INSERT: the equality one writes the audit
    // row (its WHEN folds true), the inequality one does not.
    denied_with(
        &db,
        "qp_r3_vc_app",
        "INSERT INTO qp_r3_vc_t VALUES (1)",
        "permission denied for table qp_r3_vc_audit",
    );
    // Only the inequality trigger: the truncation makes it false.
    allowed(&db, "qp_r3_vc_app", "INSERT INTO qp_r3_vc_t2 VALUES (1)");
}

#[test]
fn assignments_that_may_write_a_tg_variable_make_it_unknown() {
    // `SELECT … INTO tg_op`, `GET DIAGNOSTICS` and a `FOR tg_op IN …` loop
    // write the promise variable through a row target, which must
    // invalidate its firing value (PL/pgSQL's exec_move_row).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_tg_app;
         CREATE TABLE qp_r3_tg_t (a int);
         CREATE TABLE qp_r3_tg_u (a int);
         CREATE TABLE qp_r3_tg_audit (a int);
         CREATE FUNCTION qp_r3_tg_into() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           SELECT 'INSERT' INTO TG_OP;
           IF TG_OP = 'INSERT' THEN INSERT INTO qp_r3_tg_audit VALUES (1); END IF;
           RETURN NEW;
         END $$;
         CREATE FUNCTION qp_r3_tg_for() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE i int;
         BEGIN
           FOR TG_OP IN SELECT 'INSERT' LOOP
             INSERT INTO qp_r3_tg_audit VALUES (1);
           END LOOP;
           RETURN NEW;
         END $$;
         CREATE TRIGGER qp_r3_tg_into_trg BEFORE INSERT ON qp_r3_tg_t
             FOR EACH ROW EXECUTE FUNCTION qp_r3_tg_into();
         CREATE TRIGGER qp_r3_tg_for_trg BEFORE INSERT ON qp_r3_tg_u
             FOR EACH ROW EXECUTE FUNCTION qp_r3_tg_for();
         GRANT INSERT ON qp_r3_tg_t, qp_r3_tg_u TO qp_r3_tg_app;",
    )]);
    // Both assign `INSERT`, so the branch runs and the audit write fails.
    denied_with(
        &db,
        "qp_r3_tg_app",
        "INSERT INTO qp_r3_tg_t VALUES (1)",
        "permission denied for table qp_r3_tg_audit",
    );
    denied_with(
        &db,
        "qp_r3_tg_app",
        "INSERT INTO qp_r3_tg_u VALUES (1)",
        "permission denied for table qp_r3_tg_audit",
    );
}

#[test]
fn record_shapes_are_flow_sensitive() {
    // A record takes each query's shape in turn: a statement before a later
    // assignment resolves against the shape it sees, not the final one.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_rec_app;
         CREATE FUNCTION qp_r3_rec_gate(int) RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         CREATE FUNCTION qp_r3_rec_gate(text) RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r3_rec_gate(text) FROM PUBLIC;
         CREATE FUNCTION qp_r3_rec_f() RETURNS int LANGUAGE plpgsql AS $$
         DECLARE r record;
         BEGIN
           SELECT 1 AS x INTO r;
           PERFORM qp_r3_rec_gate(r.x);
           SELECT 'a'::text AS x INTO r;
           RETURN 1;
         END $$;
         GRANT EXECUTE ON FUNCTION qp_r3_rec_gate(int), qp_r3_rec_f() TO qp_r3_rec_app;
         CREATE FUNCTION qp_r3_rec_bad() RETURNS int LANGUAGE plpgsql AS $$
         DECLARE r record;
         BEGIN
           SELECT 1 AS x INTO r;
           SELECT 'a'::text AS x INTO r;
           PERFORM qp_r3_rec_gate(r.x);
           RETURN 1;
         END $$;
         GRANT EXECUTE ON FUNCTION qp_r3_rec_bad() TO qp_r3_rec_app;",
    )]);
    // The first call sees integer: gate(int) only.
    allowed(&db, "qp_r3_rec_app", "SELECT qp_r3_rec_f()");
    // The second sees text: gate(text) is required.
    denied_with(
        &db,
        "qp_r3_rec_app",
        "SELECT qp_r3_rec_bad()",
        "permission denied for function qp_r3_rec_gate",
    );
}

#[test]
fn cte_reachability_follows_live_references() {
    // A reference the planner folds away doesn't plan the CTE it names.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_cte_app;
         CREATE TABLE qp_r3_cte_secret (id int);",
    )]);
    allowed(
        &db,
        "qp_r3_cte_app",
        "WITH a AS (SELECT id FROM qp_r3_cte_secret),
         b AS (SELECT CASE WHEN false THEN (SELECT id FROM a) ELSE 1 END AS id)
         SELECT * FROM b",
    );
    // A row the plan certainly never computes doesn't reach the CTE either.
    allowed(
        &db,
        "qp_r3_cte_app",
        "WITH c AS (SELECT id FROM qp_r3_cte_secret)
         SELECT 1 WHERE false AND EXISTS (SELECT 1 FROM c WHERE true)",
    );
    // An EXISTS the level really plans does.
    denied_with(
        &db,
        "qp_r3_cte_app",
        "WITH c AS (SELECT id FROM qp_r3_cte_secret)
         SELECT 1 WHERE EXISTS (SELECT 1 FROM c WHERE true)",
        "permission denied for table qp_r3_cte_secret",
    );
}

#[test]
fn stored_expression_pruning_identifies_callees_by_oid() {
    // A live overload of a name must not resurrect a dead overload's
    // dependency: the default's dead CASE branch calls `f()`, the live one
    // `f(1)`.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_oid_app;
         CREATE FUNCTION qp_r3_oid_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 7; END $$;
         CREATE FUNCTION qp_r3_oid_f(int) RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r3_oid_f() FROM PUBLIC;
         CREATE TABLE qp_r3_oid_t (a int DEFAULT
             CASE WHEN false THEN qp_r3_oid_f() ELSE qp_r3_oid_f(1) END);
         GRANT INSERT ON qp_r3_oid_t TO qp_r3_oid_app;",
    )]);
    allowed(
        &db,
        "qp_r3_oid_app",
        "INSERT INTO qp_r3_oid_t DEFAULT VALUES",
    );
}

#[test]
fn routine_defaults_drop_dead_branches() {
    // A parameter DEFAULT's dead CASE branch runs no EXECUTE check
    // (add_function_defaults + eval_const_expressions).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_rd_app;
         CREATE FUNCTION qp_r3_rd_secret() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 7; END $$;
         CREATE FUNCTION qp_r3_rd_f(a int DEFAULT
             CASE WHEN false THEN qp_r3_rd_secret() ELSE 1 END)
             RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r3_rd_secret() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_r3_rd_f(int) TO qp_r3_rd_app;",
    )]);
    allowed(&db, "qp_r3_rd_app", "SELECT qp_r3_rd_f()");
}

#[test]
fn simple_case_values_are_not_conditions() {
    // `CASE false WHEN true THEN nextval('s') ELSE 1 END` compares its
    // subject with the WHEN value: the THEN branch doesn't run.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_case_app;
         CREATE SEQUENCE qp_r3_case_s;
         CREATE TABLE qp_r3_case_off (a bigint DEFAULT
             CASE false WHEN true THEN nextval('qp_r3_case_s') ELSE 1 END);
         CREATE TABLE qp_r3_case_on (a bigint DEFAULT
             CASE false WHEN false THEN nextval('qp_r3_case_s') ELSE 1 END);
         GRANT INSERT ON qp_r3_case_off, qp_r3_case_on TO qp_r3_case_app;",
    )]);
    allowed(
        &db,
        "qp_r3_case_app",
        "INSERT INTO qp_r3_case_off DEFAULT VALUES",
    );
    denied_with(
        &db,
        "qp_r3_case_app",
        "INSERT INTO qp_r3_case_on DEFAULT VALUES",
        "permission denied for sequence qp_r3_case_s",
    );
}

#[test]
fn a_default_already_of_the_domain_type_is_not_coerced() {
    // coerce_type leaves a value already of the column's type unchanged: a
    // default returning the destination domain runs no CoerceToDomain, so
    // the domain's CHECK needs no EXECUTE at the call site.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_dom_app;
         CREATE FUNCTION qp_r3_dom_guard(int) RETURNS boolean LANGUAGE plpgsql
             AS $$ BEGIN RETURN true; END $$;
         CREATE DOMAIN qp_r3_dom_d AS int CHECK (qp_r3_dom_guard(VALUE));
         CREATE TABLE qp_r3_dom_src (a qp_r3_dom_d);
         INSERT INTO qp_r3_dom_src VALUES (1);
         CREATE FUNCTION qp_r3_dom_get() RETURNS qp_r3_dom_d LANGUAGE sql SECURITY DEFINER
             AS 'SELECT a FROM qp_r3_dom_src LIMIT 1';
         CREATE TABLE qp_r3_dom_t (a qp_r3_dom_d DEFAULT qp_r3_dom_get());
         REVOKE EXECUTE ON FUNCTION qp_r3_dom_guard(int) FROM PUBLIC;
         GRANT INSERT ON qp_r3_dom_t TO qp_r3_dom_app;
         GRANT EXECUTE ON FUNCTION qp_r3_dom_get() TO qp_r3_dom_app;",
    )]);
    allowed(
        &db,
        "qp_r3_dom_app",
        "INSERT INTO qp_r3_dom_t DEFAULT VALUES",
    );
}

#[test]
fn insert_routing_only_checks_the_partitions_reached() {
    // ExecFindPartition routes a literal row to one partition; the plan's
    // result relations (and so their generation expressions and row
    // triggers) are pruned to it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_part_app;
         CREATE FUNCTION qp_r3_part_secret(int) RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN $1; END $$;
         CREATE TABLE qp_r3_part_p (
             k int,
             g int GENERATED ALWAYS AS (k) STORED
         ) PARTITION BY LIST (k);
         CREATE TABLE qp_r3_part_p1 PARTITION OF qp_r3_part_p FOR VALUES IN (1);
         CREATE TABLE qp_r3_part_p2 PARTITION OF qp_r3_part_p
             (g GENERATED ALWAYS AS (qp_r3_part_secret(k)) STORED)
             FOR VALUES IN (2);
         REVOKE EXECUTE ON FUNCTION qp_r3_part_secret(int) FROM PUBLIC;
         GRANT INSERT ON qp_r3_part_p TO qp_r3_part_app;
         CREATE TABLE qp_r3_part_audit (a int);
         CREATE FUNCTION qp_r3_part_tr() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN INSERT INTO qp_r3_part_audit VALUES (1); RETURN NEW; END $$;
         CREATE TABLE qp_r3_part_q (k int) PARTITION BY LIST (k);
         CREATE TABLE qp_r3_part_q1 PARTITION OF qp_r3_part_q FOR VALUES IN (1);
         CREATE TABLE qp_r3_part_q2 PARTITION OF qp_r3_part_q FOR VALUES IN (2);
         CREATE TRIGGER qp_r3_part_q2_tr BEFORE INSERT ON qp_r3_part_q2
             FOR EACH ROW EXECUTE FUNCTION qp_r3_part_tr();
         GRANT INSERT ON qp_r3_part_q TO qp_r3_part_app;",
    )]);
    allowed(
        &db,
        "qp_r3_part_app",
        "INSERT INTO qp_r3_part_p (k) VALUES (1)",
    );
    denied_with(
        &db,
        "qp_r3_part_app",
        "INSERT INTO qp_r3_part_p (k) VALUES (2)",
        "permission denied for function qp_r3_part_secret",
    );
    // Rows reaching several partitions keep all of them.
    denied_with(
        &db,
        "qp_r3_part_app",
        "INSERT INTO qp_r3_part_p (k) VALUES (1), (2)",
        "permission denied for function qp_r3_part_secret",
    );
    allowed(
        &db,
        "qp_r3_part_app",
        "INSERT INTO qp_r3_part_q (k) VALUES (1)",
    );
    denied_with(
        &db,
        "qp_r3_part_app",
        "INSERT INTO qp_r3_part_q (k) VALUES (2)",
        "permission denied for table qp_r3_part_audit",
    );
}

#[test]
fn an_unrelated_deferrable_trigger_does_not_disable_catching() {
    // A deferred constraint trigger escapes the handler of the statement
    // that fires it, for its event: an UPDATE trigger on the table doesn't
    // escape an INSERT statement's handler, and
    // `DEFERRABLE INITIALLY IMMEDIATE` isn't deferred at all.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_def_app;
         CREATE TABLE qp_r3_def_t (a int);
         CREATE TABLE qp_r3_def_secret (a int);
         CREATE FUNCTION qp_r3_def_tr() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN RETURN NEW; END $$;
         CREATE CONSTRAINT TRIGGER qp_r3_def_deferred AFTER UPDATE ON qp_r3_def_t
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION qp_r3_def_tr();
         CREATE CONSTRAINT TRIGGER qp_r3_def_immediate AFTER UPDATE ON qp_r3_def_t
             DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION qp_r3_def_tr();
         CREATE FUNCTION qp_r3_def_f() RETURNS int LANGUAGE plpgsql AS $$
         BEGIN
           BEGIN
             INSERT INTO qp_r3_def_t VALUES (1);
             PERFORM a FROM qp_r3_def_secret;
           EXCEPTION WHEN insufficient_privilege THEN NULL;
           END;
           RETURN 1;
         END $$;
         GRANT INSERT ON qp_r3_def_t TO qp_r3_def_app;
         GRANT EXECUTE ON FUNCTION qp_r3_def_f() TO qp_r3_def_app;",
    )]);
    allowed(&db, "qp_r3_def_app", "SELECT qp_r3_def_f()");
}

#[test]
fn pg_user_lists_only_login_roles() {
    // `pg_user` selects from `pg_shadow`, which filters `rolcanlogin`;
    // `pg_roles` has every role. A DO block's role-existence test must use
    // the view it queries.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_do_app;
         CREATE ROLE qp_r3_do_witness NOLOGIN;
         CREATE TABLE qp_r3_do_t (a int);
         DO $$
         BEGIN
           IF NOT EXISTS (SELECT FROM pg_catalog.pg_user WHERE usename = 'qp_r3_do_witness')
           THEN
             GRANT SELECT ON qp_r3_do_t TO qp_r3_do_app;
           END IF;
         END $$;",
    )]);
    allowed(&db, "qp_r3_do_app", "SELECT * FROM qp_r3_do_t");
}

#[test]
fn drop_owned_by_several_roles_checks_them_together() {
    // shdepDropOwned collects every role's objects into one set before
    // performMultipleDeletions: a dependency between two of the roles'
    // objects doesn't block the command.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_own_app;
         CREATE ROLE qp_r3_own_a;
         CREATE ROLE qp_r3_own_b;
         CREATE TABLE qp_r3_own_t (id int);
         ALTER TABLE qp_r3_own_t OWNER TO qp_r3_own_a;
         CREATE VIEW qp_r3_own_v AS SELECT * FROM qp_r3_own_t;
         ALTER VIEW qp_r3_own_v OWNER TO qp_r3_own_b;
         DROP OWNED BY qp_r3_own_a, qp_r3_own_b;",
    )]);
    allowed(&db, "qp_r3_own_app", "SELECT 1");
}

#[test]
fn coalesce_keeps_arguments_after_a_folded_call_of_unknown_value() {
    // evaluate_function folds an immutable call over constants to a Const
    // whose value — NULL or not — the analyzer doesn't know: the arguments
    // after it may still run.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_coal_app;
         CREATE FUNCTION qp_r3_coal_nuller() RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN NULL; END $$;
         CREATE FUNCTION qp_r3_coal_secret() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 7; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r3_coal_secret() FROM PUBLIC;",
    )]);
    denied_with(
        &db,
        "qp_r3_coal_app",
        "SELECT COALESCE(qp_r3_coal_nuller(), qp_r3_coal_secret())",
        "permission denied for function qp_r3_coal_secret",
    );
    // A literal constant still stops it.
    allowed(
        &db,
        "qp_r3_coal_app",
        "SELECT COALESCE(1, qp_r3_coal_secret())",
    );
}

#[test]
fn declaration_initializers_are_not_caught_by_their_own_handler() {
    // exec_stmt_block initializes the block's variables before it opens the
    // handler's protected subtransaction.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_init_app;
         CREATE FUNCTION qp_r3_init_secret() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 7; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r3_init_secret() FROM PUBLIC;
         CREATE FUNCTION qp_r3_init_f() RETURNS int LANGUAGE plpgsql AS $$
         DECLARE n int := qp_r3_init_secret();
         BEGIN
           RETURN 1;
         EXCEPTION WHEN insufficient_privilege THEN
           RETURN 1;
         END $$;
         GRANT EXECUTE ON FUNCTION qp_r3_init_f() TO qp_r3_init_app;",
    )]);
    denied_with(
        &db,
        "qp_r3_init_app",
        "SELECT qp_r3_init_f()",
        "permission denied for function qp_r3_init_secret",
    );
}

#[test]
fn return_query_execute_evaluates_its_query_expression() {
    // exec_stmt_return_query evaluates the `dynquery` expression before it
    // prepares the query.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_rq_app;
         CREATE FUNCTION qp_r3_rq_text() RETURNS text LANGUAGE plpgsql
             AS $$ BEGIN RETURN 'SELECT 1'; END $$;
         REVOKE EXECUTE ON FUNCTION qp_r3_rq_text() FROM PUBLIC;
         CREATE FUNCTION qp_r3_rq_f() RETURNS SETOF int LANGUAGE plpgsql AS $$
         BEGIN
           RETURN QUERY EXECUTE qp_r3_rq_text();
         END $$;
         GRANT EXECUTE ON FUNCTION qp_r3_rq_f() TO qp_r3_rq_app;",
    )]);
    denied_with(
        &db,
        "qp_r3_rq_app",
        "SELECT * FROM qp_r3_rq_f()",
        "permission denied for function qp_r3_rq_text",
    );
}

#[test]
fn stored_check_sequences_need_their_privileges() {
    // A CHECK's `nextval` runs with the writing role's sequence privileges,
    // whether or not the expression analysis found the call.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_chk_app;
         CREATE SEQUENCE qp_r3_chk_s;
         CREATE TABLE qp_r3_chk_t (a int CHECK (nextval('qp_r3_chk_s') > 0));
         GRANT INSERT ON qp_r3_chk_t TO qp_r3_chk_app;",
    )]);
    denied_with(
        &db,
        "qp_r3_chk_app",
        "INSERT INTO qp_r3_chk_t VALUES (1)",
        "permission denied for sequence qp_r3_chk_s",
    );
}

#[test]
fn domain_check_builtins_need_execute() {
    // ExecInitCoerceToDomain initializes the domain's constraint
    // expressions: a pinned built-in's EXECUTE is checked even though
    // `pg_depend` records no edge for it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_dchk_app;
         CREATE DOMAIN qp_r3_dchk_d AS int CHECK (pg_catalog.abs(VALUE) >= 0);
         REVOKE EXECUTE ON FUNCTION pg_catalog.abs(int) FROM PUBLIC;",
    )]);
    denied_with(
        &db,
        "qp_r3_dchk_app",
        "SELECT 1::qp_r3_dchk_d",
        "permission denied for function abs",
    );
}

#[test]
fn an_empty_global_default_acl_behaves_like_the_hardwired_default() {
    // PG keeps the emptied `pg_default_acl` row, but `get_user_default_acl`
    // merges through `aclmerge`, which returns NULL for an empty ACL: a
    // function created afterwards keeps the hard-wired default (PUBLIC
    // EXECUTE). Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_r3_dacl_app;
         CREATE ROLE qp_r3_dacl_owner;
         CREATE ROLE qp_r3_dacl_removed;
         GRANT CREATE ON SCHEMA public TO qp_r3_dacl_owner;
         ALTER DEFAULT PRIVILEGES FOR ROLE qp_r3_dacl_owner
             REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES FOR ROLE qp_r3_dacl_owner
             REVOKE EXECUTE ON FUNCTIONS FROM qp_r3_dacl_owner;
         ALTER DEFAULT PRIVILEGES FOR ROLE qp_r3_dacl_owner
             GRANT EXECUTE ON FUNCTIONS TO qp_r3_dacl_removed;
         DROP OWNED BY qp_r3_dacl_removed;
         SET ROLE qp_r3_dacl_owner;
         CREATE FUNCTION qp_r3_dacl_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         RESET ROLE;",
    )]);
    allowed(&db, "qp_r3_dacl_app", "SELECT qp_r3_dacl_f()");
}

// ──────────────────────────────────────────────────────────────────────────────
// Fourth round (T9): false denials from realistic applications
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn group_and_limit_outputs_of_a_view_unused_by_the_query_are_not_evaluated() {
    // `remove_unused_subquery_outputs` (allpaths.c) works per target entry
    // even when the subquery groups, sorts or limits: it bails out only for
    // a set operation and a plain DISTINCT. An output the upper query does
    // not read is replaced by a NULL constant unless it has a
    // `ressortgroupref` (GROUP BY / ORDER BY / DISTINCT ON / a window's
    // PARTITION BY or ORDER BY names it) or is volatile or contains a
    // set-returning call. A HAVING / WHERE / GROUP BY expression is never
    // removed.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t9_vc_app;
         CREATE TABLE qp_t9_vc_t (id int, v text, g int);
         CREATE FUNCTION qp_t9_vc_mask(text) RETURNS text LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN left($1, 1) || '***'; END $$;
         CREATE FUNCTION qp_t9_vc_vmask(text) RETURNS text LANGUAGE plpgsql VOLATILE
             AS $$ BEGIN RETURN left($1, 1) || '***'; END $$;
         CREATE FUNCTION qp_t9_vc_srf(text) RETURNS SETOF text LANGUAGE sql IMMUTABLE
             AS $$ SELECT $1 UNION ALL SELECT $1 $$;
         REVOKE EXECUTE ON FUNCTION qp_t9_vc_mask(text), qp_t9_vc_vmask(text),
             qp_t9_vc_srf(text) FROM PUBLIC;
         CREATE VIEW qp_t9_vc_grp AS
             SELECT g, count(*) AS n, max(qp_t9_vc_mask(v)) AS m
             FROM qp_t9_vc_t GROUP BY g;
         CREATE VIEW qp_t9_vc_lim AS
             SELECT id, qp_t9_vc_mask(v) AS v FROM qp_t9_vc_t ORDER BY id LIMIT 10;
         CREATE VIEW qp_t9_vc_diston AS
             SELECT DISTINCT ON (id) id, qp_t9_vc_mask(v) AS v FROM qp_t9_vc_t;
         CREATE VIEW qp_t9_vc_dist AS
             SELECT DISTINCT id, qp_t9_vc_mask(v) AS v FROM qp_t9_vc_t;
         CREATE VIEW qp_t9_vc_ord AS
             SELECT id, qp_t9_vc_mask(v) AS v FROM qp_t9_vc_t ORDER BY v;
         CREATE VIEW qp_t9_vc_grpexpr AS
             SELECT g, qp_t9_vc_mask(v) AS m FROM qp_t9_vc_t
             GROUP BY g, qp_t9_vc_mask(v);
         CREATE VIEW qp_t9_vc_having AS
             SELECT g, max(qp_t9_vc_mask(v)) AS m FROM qp_t9_vc_t GROUP BY g
             HAVING max(qp_t9_vc_mask(v)) IS NOT NULL;
         CREATE VIEW qp_t9_vc_where AS
             SELECT id, qp_t9_vc_mask(v) AS m FROM qp_t9_vc_t
             WHERE qp_t9_vc_mask(v) IS NOT NULL;
         CREATE VIEW qp_t9_vc_vol AS
             SELECT id, qp_t9_vc_vmask(v) AS v FROM qp_t9_vc_t;
         CREATE VIEW qp_t9_vc_srf AS
             SELECT id, qp_t9_vc_srf(v) AS s FROM qp_t9_vc_t;
         CREATE VIEW qp_t9_vc_win AS
             SELECT id, first_value(qp_t9_vc_mask(v)) OVER (PARTITION BY g) AS w
             FROM qp_t9_vc_t;
         CREATE VIEW qp_t9_vc_win_key AS
             SELECT id, row_number() OVER (ORDER BY qp_t9_vc_mask(v)) AS w
             FROM qp_t9_vc_t;
         CREATE VIEW qp_t9_vc_union AS
             SELECT id, qp_t9_vc_mask(v) AS v FROM qp_t9_vc_t
             UNION SELECT id, v FROM qp_t9_vc_t;
         CREATE VIEW qp_t9_vc_ord2 AS
             SELECT id, qp_t9_vc_mask(v) AS m FROM qp_t9_vc_t ORDER BY 2;
         CREATE VIEW qp_t9_vc_ord1 AS
             SELECT id, qp_t9_vc_mask(v) AS m FROM qp_t9_vc_t ORDER BY 1;
         CREATE VIEW qp_t9_vc_alias AS
             SELECT id, qp_t9_vc_mask(v) AS m FROM qp_t9_vc_t ORDER BY m;
         CREATE VIEW qp_t9_vc_sb WITH (security_barrier) AS
             SELECT id, qp_t9_vc_mask(v) AS v FROM qp_t9_vc_t;
         GRANT SELECT ON qp_t9_vc_t, qp_t9_vc_grp, qp_t9_vc_lim,
             qp_t9_vc_diston, qp_t9_vc_dist, qp_t9_vc_ord, qp_t9_vc_grpexpr,
             qp_t9_vc_having, qp_t9_vc_where, qp_t9_vc_vol, qp_t9_vc_srf,
             qp_t9_vc_win, qp_t9_vc_win_key, qp_t9_vc_union, qp_t9_vc_ord2,
             qp_t9_vc_ord1, qp_t9_vc_alias, qp_t9_vc_sb TO qp_t9_vc_app;",
    )]);
    // The unused output's aggregate is dropped from the grouped view.
    allowed(&db, "qp_t9_vc_app", "SELECT g, n FROM qp_t9_vc_grp");
    allowed(
        &db,
        "qp_t9_vc_app",
        "SELECT g, n FROM qp_t9_vc_grp ORDER BY n",
    );
    allowed(&db, "qp_t9_vc_app", "SELECT count(*) FROM qp_t9_vc_grp");
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT m FROM qp_t9_vc_grp",
        "permission denied for function qp_t9_vc_mask",
    );
    // ORDER BY / LIMIT does not keep the output the query doesn't read.
    allowed(&db, "qp_t9_vc_app", "SELECT id FROM qp_t9_vc_lim");
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT v FROM qp_t9_vc_lim",
        "permission denied for function qp_t9_vc_mask",
    );
    // DISTINCT ON keeps its key's output only; plain DISTINCT keeps all.
    allowed(&db, "qp_t9_vc_app", "SELECT id FROM qp_t9_vc_diston");
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT id FROM qp_t9_vc_dist",
        "permission denied for function qp_t9_vc_mask",
    );
    // A sort key naming an output (or a GROUP BY key expression) is kept.
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT id FROM qp_t9_vc_ord",
        "permission denied for function qp_t9_vc_mask",
    );
    for sql in [
        "SELECT g FROM qp_t9_vc_grpexpr",
        "SELECT m FROM qp_t9_vc_grpexpr",
        "SELECT g FROM qp_t9_vc_having",
        "SELECT id FROM qp_t9_vc_where",
    ] {
        denied_with(
            &db,
            "qp_t9_vc_app",
            sql,
            "permission denied for function qp_t9_vc_mask",
        );
    }
    // A volatile output, a set-returning one and a set operation's are kept.
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT id FROM qp_t9_vc_vol",
        "permission denied for function qp_t9_vc_vmask",
    );
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT id FROM qp_t9_vc_srf",
        "permission denied for function qp_t9_vc_srf",
    );
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT id FROM qp_t9_vc_union",
        "permission denied for function qp_t9_vc_mask",
    );
    // A window function's own arguments are dropped with its unused output;
    // its PARTITION BY / ORDER BY keys are not.
    allowed(&db, "qp_t9_vc_app", "SELECT id FROM qp_t9_vc_win");
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT id FROM qp_t9_vc_win_key",
        "permission denied for function qp_t9_vc_mask",
    );
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT count(*) FROM qp_t9_vc_win_key",
        "permission denied for function qp_t9_vc_mask",
    );
    // An ordinal or an output alias in ORDER BY marks the output it
    // names.
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT id FROM qp_t9_vc_ord2",
        "permission denied for function qp_t9_vc_mask",
    );
    allowed(&db, "qp_t9_vc_app", "SELECT id FROM qp_t9_vc_ord1");
    denied_with(
        &db,
        "qp_t9_vc_app",
        "SELECT count(*) FROM qp_t9_vc_alias",
        "permission denied for function qp_t9_vc_mask",
    );
    // A security_barrier view is not pulled up, but its unused outputs are
    // still removed.
    allowed(&db, "qp_t9_vc_app", "SELECT id FROM qp_t9_vc_sb");
    // A FROM subquery planned on its own is the same case
    // (`remove_unused_subquery_outputs`).
    allowed(
        &db,
        "qp_t9_vc_app",
        "SELECT id FROM (SELECT id, qp_t9_vc_mask(v) AS m FROM qp_t9_vc_t
             ORDER BY id LIMIT 10) s",
    );
    allowed(
        &db,
        "qp_t9_vc_app",
        "SELECT g, n FROM (SELECT g, count(*) AS n, max(qp_t9_vc_mask(v)) AS m
             FROM qp_t9_vc_t GROUP BY g) s",
    );
}

#[test]
fn security_invoker_view_unused_sublink_output_reads_no_relation() {
    // A view's unused output is `remove_unused_subquery_outputs`' business
    // whatever its owner: for a `security_invoker` view the relations of
    // the outputs the query reads are checked as the caller — the ones of
    // an output it doesn't read must not be expanded at all. `SELECT *`
    // still reads the sublink.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t9_sl_app;
         CREATE TABLE qp_t9_sl_posts (id int, title text);
         CREATE TABLE qp_t9_sl_likes (post_id int);
         CREATE VIEW qp_t9_sl_list WITH (security_invoker) AS
             SELECT p.id, p.title,
                    (SELECT count(*) FROM qp_t9_sl_likes l WHERE l.post_id = p.id) AS likes
             FROM qp_t9_sl_posts p;
         CREATE VIEW qp_t9_sl_barrier WITH (security_invoker, security_barrier) AS
             SELECT p.id, p.title,
                    (SELECT count(*) FROM qp_t9_sl_likes l WHERE l.post_id = p.id) AS likes
             FROM qp_t9_sl_posts p;
         GRANT SELECT ON qp_t9_sl_posts, qp_t9_sl_list, qp_t9_sl_barrier TO qp_t9_sl_app;",
    )]);
    allowed(&db, "qp_t9_sl_app", "SELECT id, title FROM qp_t9_sl_list");
    allowed(
        &db,
        "qp_t9_sl_app",
        "SELECT id, title FROM qp_t9_sl_barrier",
    );
    allowed(
        &db,
        "qp_t9_sl_app",
        "SELECT id FROM (SELECT p.id,
             (SELECT count(*) FROM qp_t9_sl_likes l WHERE l.post_id = p.id) AS n
             FROM qp_t9_sl_posts p) s",
    );
    denied_with(
        &db,
        "qp_t9_sl_app",
        "SELECT * FROM qp_t9_sl_list",
        "permission denied for table qp_t9_sl_likes",
    );
    denied_with(
        &db,
        "qp_t9_sl_app",
        "SELECT likes FROM qp_t9_sl_list",
        "permission denied for table qp_t9_sl_likes",
    );
}

#[test]
fn explain_of_a_sql_function_body_only_plans_what_the_planner_inlines_or_folds() {
    // EXPLAIN without ANALYZE starts the executor and never runs it. A
    // routine's body is reached only when the planner plans it into this
    // statement: `inline_function` / `inline_set_returning_function`
    // substitute an inlinable SQL body, and `evaluate_function` executes an
    // IMMUTABLE call over constant arguments while folding. A VOLATILE
    // function whose body has an aggregate is neither: only its EXECUTE is
    // checked.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t9_ex_app;
         CREATE TABLE qp_t9_ex_secret (id int);
         CREATE FUNCTION qp_t9_ex_cnt() RETURNS bigint LANGUAGE sql
             AS $$ SELECT count(*) FROM qp_t9_ex_secret $$;
         CREATE FUNCTION qp_t9_ex_cnt_imm() RETURNS bigint LANGUAGE sql IMMUTABLE
             AS $$ SELECT count(*) FROM qp_t9_ex_secret $$;
         CREATE FUNCTION qp_t9_ex_srf() RETURNS SETOF int LANGUAGE sql IMMUTABLE
             AS $$ SELECT id FROM qp_t9_ex_secret $$;
         CREATE FUNCTION qp_t9_ex_srf_vol() RETURNS SETOF int LANGUAGE sql VOLATILE
             AS $$ SELECT id FROM qp_t9_ex_secret $$;
         CREATE FUNCTION qp_t9_ex_plain() RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT 1 $$;
         CREATE FUNCTION qp_t9_ex_protected() RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t9_ex_protected() FROM PUBLIC;
         CREATE FUNCTION qp_t9_ex_inlined() RETURNS int LANGUAGE sql VOLATILE
             AS $$ SELECT qp_t9_ex_protected() $$;
         CREATE TABLE qp_t9_ex_public (id int);
         CREATE FUNCTION qp_t9_ex_cnt_arg(int) RETURNS bigint LANGUAGE sql IMMUTABLE
             AS $$ SELECT count(*) FROM qp_t9_ex_secret WHERE id = $1 $$;
         CREATE FUNCTION qp_t9_ex_strict(int) RETURNS bigint LANGUAGE sql IMMUTABLE STRICT
             AS $$ SELECT count(*) FROM qp_t9_ex_secret WHERE id = $1 $$;
         CREATE FUNCTION qp_t9_ex_scan() RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT count(*)::int FROM qp_t9_ex_secret $$;
         GRANT EXECUTE ON FUNCTION qp_t9_ex_cnt(), qp_t9_ex_cnt_imm(),
             qp_t9_ex_srf(), qp_t9_ex_srf_vol(), qp_t9_ex_plain(),
             qp_t9_ex_inlined(), qp_t9_ex_cnt_arg(int), qp_t9_ex_strict(int),
             qp_t9_ex_scan() TO qp_t9_ex_app;
         GRANT SELECT ON qp_t9_ex_public TO qp_t9_ex_app;",
    )]);
    // The non-inlined body's relation is not in the plan; the plan's own
    // relations still are.
    allowed(&db, "qp_t9_ex_app", "EXPLAIN SELECT qp_t9_ex_cnt()");
    denied_with(
        &db,
        "qp_t9_ex_app",
        "EXPLAIN SELECT * FROM qp_t9_ex_secret",
        "permission denied for table qp_t9_ex_secret",
    );
    // A non-inlined set-returning function stays a Function Scan.
    allowed(
        &db,
        "qp_t9_ex_app",
        "EXPLAIN SELECT * FROM qp_t9_ex_srf_vol()",
    );
    // The inlined body's functions are initialized with the plan.
    denied_with(
        &db,
        "qp_t9_ex_app",
        "EXPLAIN SELECT qp_t9_ex_inlined()",
        "permission denied for function qp_t9_ex_protected",
    );
    // An IMMUTABLE call over constant arguments is evaluated while folding.
    denied_with(
        &db,
        "qp_t9_ex_app",
        "EXPLAIN SELECT qp_t9_ex_cnt_imm()",
        "permission denied for table qp_t9_ex_secret",
    );
    // An inlined set-returning function's body is planned.
    denied_with(
        &db,
        "qp_t9_ex_app",
        "EXPLAIN SELECT * FROM qp_t9_ex_srf()",
        "permission denied for table qp_t9_ex_secret",
    );
    // The statements themselves run the bodies: no privilege, no run.
    denied_with(
        &db,
        "qp_t9_ex_app",
        "SELECT qp_t9_ex_cnt()",
        "permission denied for table qp_t9_ex_secret",
    );
    denied_with(
        &db,
        "qp_t9_ex_app",
        "EXPLAIN ANALYZE SELECT qp_t9_ex_cnt()",
        "permission denied for table qp_t9_ex_secret",
    );
    // An inlined body over nothing needs no privilege beyond the call's.
    allowed(&db, "qp_t9_ex_app", "EXPLAIN SELECT qp_t9_ex_plain()");
    // The folding conditions: an IMMUTABLE call runs while folding only
    // over constant arguments, and a STRICT call over a NULL argument is
    // NULL without running.
    denied_with(
        &db,
        "qp_t9_ex_app",
        "EXPLAIN SELECT qp_t9_ex_cnt_arg(1)",
        "permission denied for table qp_t9_ex_secret",
    );
    allowed(
        &db,
        "qp_t9_ex_app",
        "EXPLAIN SELECT qp_t9_ex_cnt_arg(id) FROM qp_t9_ex_public",
    );
    allowed(&db, "qp_t9_ex_app", "EXPLAIN SELECT qp_t9_ex_strict(NULL)");
    denied_with(
        &db,
        "qp_t9_ex_app",
        "EXPLAIN SELECT * FROM qp_t9_ex_scan()",
        "permission denied for table qp_t9_ex_secret",
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Fifth round (T10): adversarial review of the previous two rounds
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn a_pruned_output_keeps_its_base_relations_parse_time_column_requirements() {
    // Output pruning drops what the planner drops — the output's routines,
    // sequences and sublink relations — but the parser recorded
    // `selectedCols` on every base relation's `RTEPermissionInfo` before the
    // planner ran, and `ExecCheckPermissions` still checks it. A column read
    // only by a dropped output is still required. Verified on PostgreSQL
    // 18.6: all the denials below are PG's.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_pr_app;
         CREATE ROLE qp_t10_pr_owner;
         CREATE TABLE qp_t10_pr_t (id int, visible text, secret text);
         CREATE TABLE qp_t10_pr_u (id int, secret2 text);
         GRANT SELECT (visible) ON qp_t10_pr_t TO qp_t10_pr_app;
         GRANT SELECT ON qp_t10_pr_u TO qp_t10_pr_app;
         CREATE VIEW qp_t10_pr_v AS
             SELECT upper(secret) AS unused, visible FROM qp_t10_pr_t;
         CREATE VIEW qp_t10_pr_inv WITH (security_invoker) AS
             SELECT upper(secret) AS unused, visible FROM qp_t10_pr_t;
         GRANT SELECT ON qp_t10_pr_v, qp_t10_pr_inv TO qp_t10_pr_app;
         GRANT CREATE ON SCHEMA public TO qp_t10_pr_owner;
         GRANT SELECT (visible) ON qp_t10_pr_t TO qp_t10_pr_owner;
         SET ROLE qp_t10_pr_owner;
         CREATE VIEW qp_t10_pr_owned AS
             SELECT upper(secret) AS unused, visible FROM qp_t10_pr_t;
         RESET ROLE;
         GRANT SELECT ON qp_t10_pr_owned TO qp_t10_pr_app;",
    )]);
    denied_with(
        &db,
        "qp_t10_pr_app",
        "SELECT 1 FROM (SELECT upper(secret) AS unused FROM qp_t10_pr_t) s",
        "permission denied for table qp_t10_pr_t",
    );
    denied_with(
        &db,
        "qp_t10_pr_app",
        "WITH s AS (SELECT upper(secret) AS unused FROM qp_t10_pr_t)
         SELECT 1 FROM s",
        "permission denied for table qp_t10_pr_t",
    );
    denied_with(
        &db,
        "qp_t10_pr_app",
        "WITH s AS MATERIALIZED (SELECT upper(secret) AS unused FROM qp_t10_pr_t)
         SELECT 1 FROM s",
        "permission denied for table qp_t10_pr_t",
    );
    // An outer relation's column read only inside a dropped output's
    // sublink is still a parse-time `selectedCols` entry.
    denied_with(
        &db,
        "qp_t10_pr_app",
        "SELECT 1 FROM (SELECT (SELECT count(*) FROM qp_t10_pr_u u
                                WHERE u.id = t.id) AS n
                        FROM qp_t10_pr_t t) s",
        "permission denied for table qp_t10_pr_t",
    );
    // A view's own query reads its relations as its owner: the columns it
    // selected are checked as the owner whether the reading query uses the
    // output or not.
    denied_with(
        &db,
        "qp_t10_pr_app",
        "SELECT visible FROM qp_t10_pr_owned",
        "permission denied for table qp_t10_pr_t",
    );
    denied_with(
        &db,
        "qp_t10_pr_app",
        "SELECT 1 FROM qp_t10_pr_owned",
        "permission denied for table qp_t10_pr_t",
    );
    // A security_invoker view reads them as the caller.
    denied_with(
        &db,
        "qp_t10_pr_app",
        "SELECT 1 FROM qp_t10_pr_inv",
        "permission denied for table qp_t10_pr_t",
    );
    // The relations of a dropped output's sublink are not planned, and a
    // sublink shadowing the outer table marks only its own relation.
    allowed(
        &db,
        "qp_t10_pr_app",
        "SELECT 1 FROM (SELECT (SELECT count(*) FROM qp_t10_pr_u) AS n
                        FROM qp_t10_pr_t WHERE visible IS NOT NULL) s",
    );
    allowed(
        &db,
        "qp_t10_pr_app",
        "SELECT 1 FROM (SELECT (SELECT count(*) FROM qp_t10_pr_t t2
                                WHERE t2.secret IS NOT NULL) AS n
                        FROM qp_t10_pr_t) s",
    );
    // A kept output still runs its expression.
    allowed(&db, "qp_t10_pr_app", "SELECT visible FROM qp_t10_pr_t");
}

#[test]
fn a_view_expanded_twice_checks_each_occurrences_outputs() {
    // Every occurrence of a view is copied and rewritten on its own
    // (`ApplyRetrieveRule`): an occurrence that reads a column an earlier
    // one didn't needs that column's output evaluated. Verified on
    // PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_occ_app;
         CREATE TABLE qp_t10_occ_t (id int);
         CREATE FUNCTION qp_t10_occ_hidden(int) RETURNS int
             LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t10_occ_hidden(int) FROM PUBLIC;
         CREATE VIEW qp_t10_occ_inner AS
             SELECT id, qp_t10_occ_hidden(id) AS masked FROM qp_t10_occ_t;
         CREATE VIEW qp_t10_occ_left AS SELECT id FROM qp_t10_occ_inner;
         CREATE VIEW qp_t10_occ_right AS SELECT masked FROM qp_t10_occ_inner;
         GRANT SELECT ON qp_t10_occ_left, qp_t10_occ_right TO qp_t10_occ_app;",
    )]);
    allowed(&db, "qp_t10_occ_app", "SELECT l.id FROM qp_t10_occ_left l");
    denied_with(
        &db,
        "qp_t10_occ_app",
        "SELECT l.id, r.masked
         FROM qp_t10_occ_left l CROSS JOIN qp_t10_occ_right r",
        "permission denied for function qp_t10_occ_hidden",
    );
    // The same occurrence twice with the same columns stays checked once.
    allowed(
        &db,
        "qp_t10_occ_app",
        "SELECT a.id, b.id FROM qp_t10_occ_left a CROSS JOIN qp_t10_occ_left b",
    );
    denied_with(
        &db,
        "qp_t10_occ_app",
        "SELECT r.masked FROM qp_t10_occ_right r",
        "permission denied for function qp_t10_occ_hidden",
    );
}

#[test]
fn a_strict_null_call_still_folds_its_arguments() {
    // `simplify_function` runs `eval_const_expressions` over a call's
    // arguments *before* `evaluate_function` applies the strict NULL
    // shortcut: an IMMUTABLE call over constants in them runs while
    // planning. Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_null_app;
         CREATE FUNCTION qp_t10_null_hidden(int) RETURNS int
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t10_null_hidden(int) FROM PUBLIC;
         CREATE FUNCTION qp_t10_null_volatile(int) RETURNS int
             LANGUAGE plpgsql VOLATILE AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t10_null_volatile(int) FROM PUBLIC;
         CREATE FUNCTION qp_t10_null_stable(int) RETURNS int
             LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t10_null_stable(int) FROM PUBLIC;",
    )]);
    denied_with(
        &db,
        "qp_t10_null_app",
        "SELECT mod(NULL::int, qp_t10_null_hidden(1))",
        "permission denied for function qp_t10_null_hidden",
    );
    denied_with(
        &db,
        "qp_t10_null_app",
        "EXPLAIN SELECT mod(NULL::int, qp_t10_null_hidden(1))",
        "permission denied for function qp_t10_null_hidden",
    );
    // A call the planner doesn't fold (VOLATILE) never runs: the arguments
    // of a folded strict call aren't evaluated.
    allowed(
        &db,
        "qp_t10_null_app",
        "SELECT mod(NULL::int, qp_t10_null_volatile(1))",
    );
    // A STABLE call over a constant is not folded while planning a running
    // statement, and the folded strict call never evaluates it.
    allowed(
        &db,
        "qp_t10_null_app",
        "SELECT mod(NULL::int, qp_t10_null_stable(1))",
    );
}

#[test]
fn strict_set_returning_and_record_calls_evaluate_their_arguments() {
    // `evaluate_function` refuses a set-returning or `record` result, so the
    // strict NULL shortcut isn't applied: the executor evaluates every
    // argument before the function's strictness skips the call
    // (`ExecEvalFuncArgs`, `execSRF.c`). Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_srf_app;
         CREATE FUNCTION qp_t10_srf_hidden() RETURNS int
             LANGUAGE plpgsql VOLATILE AS $$ BEGIN RETURN 3; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t10_srf_hidden() FROM PUBLIC;
         CREATE FUNCTION qp_t10_srf_rec(IN a int, IN b int, OUT x int, OUT y int)
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN x := 1; y := 2; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t10_srf_rec(int, int) FROM PUBLIC;",
    )]);
    denied_with(
        &db,
        "qp_t10_srf_app",
        "SELECT generate_series(NULL::int, qp_t10_srf_hidden())",
        "permission denied for function qp_t10_srf_hidden",
    );
    denied_with(
        &db,
        "qp_t10_srf_app",
        "EXPLAIN SELECT generate_series(NULL::int, qp_t10_srf_hidden())",
        "permission denied for function qp_t10_srf_hidden",
    );
    // A `record` result is never folded either: the call stays in the plan
    // and the executor evaluates its arguments (and the call's EXECUTE is
    // checked when the executor initializes it).
    denied_with(
        &db,
        "qp_t10_srf_app",
        "SELECT * FROM (SELECT (NULL::int, 1) AS r, qp_t10_srf_rec(1, 2) AS q) s",
        "permission denied for function qp_t10_srf_rec",
    );
}

#[test]
fn a_null_cast_to_a_domain_with_constraints_is_a_coercion() {
    // `coerce_type` builds a `Const` of the domain's *base* type and wraps
    // it in a `CoerceToDomain`; the domain's check expressions run even for
    // a NULL value (`EEOP_DOMAIN_CHECK`). A domain without constraints is
    // replaced by a `RelabelType` while folding, so its NULL stays a
    // constant. Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_dom_app;
         CREATE FUNCTION qp_t10_dom_guard(int) RETURNS boolean
             LANGUAGE plpgsql AS $$ BEGIN RETURN true; END $$;
         CREATE DOMAIN qp_t10_dom_d AS int CHECK (qp_t10_dom_guard(VALUE));
         CREATE DOMAIN qp_t10_dom_free AS int;
         REVOKE EXECUTE ON FUNCTION qp_t10_dom_guard(int) FROM PUBLIC;",
    )]);
    denied_with(
        &db,
        "qp_t10_dom_app",
        "SELECT mod(NULL::qp_t10_dom_d, 2)",
        "permission denied for function qp_t10_dom_guard",
    );
    denied_with(
        &db,
        "qp_t10_dom_app",
        "EXPLAIN SELECT mod(NULL::qp_t10_dom_d, 2)",
        "permission denied for function qp_t10_dom_guard",
    );
    allowed(
        &db,
        "qp_t10_dom_app",
        "SELECT mod(NULL::qp_t10_dom_free, 2)",
    );
}

#[test]
fn simple_case_compares_with_the_subjects_type() {
    // `transformCaseExpr` builds `subject = when` and lets operator
    // resolution coerce: an unknown string literal takes the subject's type,
    // so `CASE 1 WHEN '1'` takes the branch. Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_case_app;
         CREATE SEQUENCE qp_t10_case_s;
         CREATE TABLE qp_t10_case_t (x bigint DEFAULT
             CASE 1 WHEN '1' THEN nextval('qp_t10_case_s') ELSE 0 END);
         CREATE TABLE qp_t10_case_off (x bigint DEFAULT
             CASE 1 WHEN '2' THEN nextval('qp_t10_case_s') ELSE 0 END);
         CREATE TABLE qp_t10_case_bool (x bigint DEFAULT
             CASE true WHEN '1' THEN nextval('qp_t10_case_s') ELSE 0 END);
         GRANT INSERT ON qp_t10_case_t, qp_t10_case_off, qp_t10_case_bool
             TO qp_t10_case_app;",
    )]);
    denied_with(
        &db,
        "qp_t10_case_app",
        "INSERT INTO qp_t10_case_t DEFAULT VALUES",
        "permission denied for sequence qp_t10_case_s",
    );
    denied_with(
        &db,
        "qp_t10_case_app",
        "INSERT INTO qp_t10_case_bool DEFAULT VALUES",
        "permission denied for sequence qp_t10_case_s",
    );
    allowed(
        &db,
        "qp_t10_case_app",
        "INSERT INTO qp_t10_case_off DEFAULT VALUES",
    );
}

#[test]
fn explain_estimation_runs_constant_stable_calls_where_pg_does() {
    // `estimate_expression_value` (estimation mode) folds STABLE calls over
    // constants while estimating a restriction: the pseudoconstant side of
    // a `WHERE` / `JOIN ... ON` comparison against a relation column, and a
    // `LIMIT` / `OFFSET` bound. HAVING, ORDER BY and a pseudoconstant qual
    // are never estimated. Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_est_app;
         CREATE TABLE qp_t10_est_secret (id bigint);
         CREATE VIEW qp_t10_est_visible AS SELECT id FROM qp_t10_est_secret;
         CREATE FUNCTION qp_t10_est_count() RETURNS bigint LANGUAGE sql STABLE
             AS $$ SELECT count(*) FROM qp_t10_est_secret $$;
         GRANT SELECT ON qp_t10_est_visible TO qp_t10_est_app;",
    )]);
    for sql in [
        "EXPLAIN SELECT id FROM qp_t10_est_visible WHERE id = qp_t10_est_count()",
        "EXPLAIN SELECT id FROM qp_t10_est_visible WHERE qp_t10_est_count() = id",
        "EXPLAIN SELECT id FROM qp_t10_est_visible WHERE id > qp_t10_est_count() + 1",
        "EXPLAIN SELECT id FROM qp_t10_est_visible WHERE id IN (qp_t10_est_count(), 1)",
        "EXPLAIN SELECT id FROM qp_t10_est_visible WHERE id = ANY(ARRAY[qp_t10_est_count()])",
        "EXPLAIN SELECT a.id FROM qp_t10_est_visible a
             JOIN qp_t10_est_visible b ON a.id = qp_t10_est_count()",
        "EXPLAIN SELECT a.id FROM qp_t10_est_visible a
             LEFT JOIN qp_t10_est_visible b ON a.id = qp_t10_est_count()",
        "EXPLAIN SELECT id FROM qp_t10_est_visible LIMIT qp_t10_est_count()",
        "EXPLAIN SELECT id FROM qp_t10_est_visible OFFSET qp_t10_est_count()",
    ] {
        denied_with(
            &db,
            "qp_t10_est_app",
            sql,
            "permission denied for table qp_t10_est_secret",
        );
    }
    // Not estimated: a pseudoconstant qual, the column's own side of the
    // comparison, HAVING, ORDER BY, a window's keys, a BooleanTest of the
    // comparison, and the tree the folding drops.
    for sql in [
        "EXPLAIN SELECT id FROM qp_t10_est_visible WHERE qp_t10_est_count() > 0",
        "EXPLAIN SELECT id FROM qp_t10_est_visible WHERE id + qp_t10_est_count() = 5",
        "EXPLAIN SELECT count(*) FROM qp_t10_est_visible
             HAVING count(*) > qp_t10_est_count()",
        "EXPLAIN SELECT id FROM qp_t10_est_visible ORDER BY qp_t10_est_count()",
        "EXPLAIN SELECT id, row_number() OVER (ORDER BY qp_t10_est_count())
             FROM qp_t10_est_visible",
        "EXPLAIN SELECT id FROM qp_t10_est_visible
             WHERE (id = qp_t10_est_count()) IS NULL",
        "EXPLAIN SELECT id FROM qp_t10_est_visible
             WHERE (id = qp_t10_est_count()) IS UNKNOWN",
        "EXPLAIN SELECT id FROM qp_t10_est_visible WHERE id = qp_t10_est_count() OR true",
        "EXPLAIN SELECT id FROM qp_t10_est_visible WHERE id = qp_t10_est_count() AND false",
    ] {
        allowed(&db, "qp_t10_est_app", sql);
    }
    // A running statement checks the body anyway.
    denied_with(
        &db,
        "qp_t10_est_app",
        "SELECT id FROM qp_t10_est_visible WHERE id = qp_t10_est_count()",
        "permission denied for table qp_t10_est_secret",
    );
}

#[test]
fn record_shapes_beyond_the_combination_cap_still_check_every_overload() {
    // A record assigned on several paths has several shapes; a statement
    // reading it is checked once per combination. More combinations than
    // the analyzer tries must not silently lose the ones a denied overload
    // needs: every routine the statement's calls could resolve to by name
    // and argument count is required as well. Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_shape_app;
         CREATE FUNCTION qp_t10_shape_gate(int,int,int,int,int,int,int)
             RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
         CREATE FUNCTION qp_t10_shape_gate(text,text,text,text,text,text,text)
             RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION
             qp_t10_shape_gate(text,text,text,text,text,text,text) FROM PUBLIC;
         CREATE FUNCTION qp_t10_shape_run(flag boolean) RETURNS int
             LANGUAGE plpgsql AS $$
         DECLARE
             a record; b record; c record; d record;
             e record; f record; g record;
         BEGIN
             IF flag THEN
                 SELECT 1 AS x INTO a; SELECT 1 AS x INTO b;
                 SELECT 1 AS x INTO c; SELECT 1 AS x INTO d;
                 SELECT 1 AS x INTO e; SELECT 1 AS x INTO f;
                 SELECT 1 AS x INTO g;
             ELSE
                 SELECT 'x'::text AS x INTO a; SELECT 'x'::text AS x INTO b;
                 SELECT 'x'::text AS x INTO c; SELECT 'x'::text AS x INTO d;
                 SELECT 'x'::text AS x INTO e; SELECT 'x'::text AS x INTO f;
                 SELECT 'x'::text AS x INTO g;
             END IF;
             PERFORM qp_t10_shape_gate(a.x,b.x,c.x,d.x,e.x,f.x,g.x);
             RETURN 1;
         END $$;",
    )]);
    denied_with(
        &db,
        "qp_t10_shape_app",
        "SELECT qp_t10_shape_run(false)",
        "permission denied for function qp_t10_shape_gate",
    );
}

#[test]
fn plpgsql_declaration_types_need_usage_but_signature_types_do_not() {
    // The compiler resolves a body's declared types and `%TYPE` /
    // `%ROWTYPE` names at the first call; the signature's types were
    // resolved at CREATE FUNCTION and need no USAGE. Comments and the
    // whitespace around a qualified name must not matter. Verified on
    // PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_decl_app;
         CREATE SCHEMA qp_t10_decl_private;
         REVOKE USAGE ON SCHEMA qp_t10_decl_private FROM PUBLIC;
         CREATE TYPE qp_t10_decl_private.e AS ENUM ('x');
         CREATE TABLE qp_t10_decl_private.t (id int, c qp_t10_decl_private.e);
         CREATE FUNCTION qp_t10_decl_missed(a qp_t10_decl_private.e) RETURNS int
           LANGUAGE plpgsql AS $$
           DECLARE v qp_t10_decl_private . e;
           BEGIN RETURN 1; END $$;
         CREATE FUNCTION qp_t10_decl_rowtype(a int) RETURNS int
           LANGUAGE plpgsql AS $$
           DECLARE r qp_t10_decl_private.t%ROWTYPE;
           BEGIN RETURN a; END $$;
         CREATE FUNCTION qp_t10_decl_coltype(a int) RETURNS int
           LANGUAGE plpgsql AS $$
           DECLARE v qp_t10_decl_private.t.c%TYPE;
           BEGIN RETURN a; END $$;
         CREATE FUNCTION qp_t10_decl_false(a qp_t10_decl_private.e) RETURNS int
           LANGUAGE plpgsql AS $$
           BEGIN
             -- qp_t10_decl_private.e is the signature's input type.
             RETURN 1; END $$;
         CREATE FUNCTION qp_t10_decl_dead(a int) RETURNS int
           LANGUAGE plpgsql AS $$
           BEGIN IF false THEN PERFORM qp_t10_decl_private.f(); END IF;
             RETURN a; END $$;",
    )]);
    for sql in [
        "SELECT qp_t10_decl_missed(NULL)",
        "SELECT qp_t10_decl_rowtype(1)",
        "SELECT qp_t10_decl_coltype(1)",
    ] {
        denied_with(
            &db,
            "qp_t10_decl_app",
            sql,
            "permission denied for schema qp_t10_decl_private",
        );
    }
    for sql in [
        "SELECT qp_t10_decl_false(NULL)",
        "SELECT qp_t10_decl_dead(1)",
    ] {
        allowed(&db, "qp_t10_decl_app", sql);
    }
}

#[test]
fn explain_output_buckets_keep_the_statements_execution_mode() {
    // EXPLAIN without ANALYZE starts the executor and runs no expression: a
    // routine body is reached only when the planner inlines it or folds the
    // call by running it. An output bucket's requirements are merged after
    // the bucket is flushed; that merge must not leave the statement
    // "running". Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_exp_app;
         CREATE TABLE qp_t10_exp_secret (id int);
         CREATE FUNCTION qp_t10_exp_count() RETURNS bigint LANGUAGE sql STABLE
             AS $$ SELECT count(*) FROM qp_t10_exp_secret $$;",
    )]);
    allowed(&db, "qp_t10_exp_app", "EXPLAIN SELECT qp_t10_exp_count()");
    allowed(
        &db,
        "qp_t10_exp_app",
        "EXPLAIN SELECT 1 AS a, qp_t10_exp_count() AS b",
    );
    allowed(
        &db,
        "qp_t10_exp_app",
        "EXPLAIN SELECT qp_t10_exp_count() ORDER BY 1",
    );
    // The statement that runs it checks the body.
    denied_with(
        &db,
        "qp_t10_exp_app",
        "SELECT qp_t10_exp_count()",
        "permission denied for table qp_t10_exp_secret",
    );
    denied_with(
        &db,
        "qp_t10_exp_app",
        "EXPLAIN ANALYZE SELECT qp_t10_exp_count()",
        "permission denied for table qp_t10_exp_secret",
    );
}

#[test]
fn a_simple_union_all_view_drops_its_unused_outputs() {
    // `pull_up_simple_union_all` turns an all-UNION ALL subquery into an
    // append relation (no ORDER BY / LIMIT / locking / WITH, no arm
    // coercion): each leaf is planned on its own and its unused outputs are
    // dropped. UNION / INTERSECT / EXCEPT, a coercion between the arms, and
    // DISTINCT keep everything. Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_ua_app;
         CREATE TABLE qp_t10_ua_t1 (id int, v text);
         CREATE TABLE qp_t10_ua_t2 (id int, v text);
         CREATE FUNCTION qp_t10_ua_one(text) RETURNS text
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN $1; END $$;
         CREATE FUNCTION qp_t10_ua_two(text) RETURNS text
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN $1; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t10_ua_one(text), qp_t10_ua_two(text)
             FROM PUBLIC;
         CREATE VIEW qp_t10_ua_union_all AS
             SELECT id, qp_t10_ua_one(v) AS v FROM qp_t10_ua_t1
             UNION ALL SELECT id, qp_t10_ua_two(v) FROM qp_t10_ua_t2;
         CREATE VIEW qp_t10_ua_union AS
             SELECT id, qp_t10_ua_one(v) AS v FROM qp_t10_ua_t1
             UNION SELECT id, qp_t10_ua_two(v) FROM qp_t10_ua_t2;
         CREATE VIEW qp_t10_ua_order AS
             SELECT id, qp_t10_ua_one(v) AS v FROM qp_t10_ua_t1
             UNION ALL SELECT id, qp_t10_ua_two(v) FROM qp_t10_ua_t2
             ORDER BY id;
         CREATE VIEW qp_t10_ua_coerced AS
             SELECT id, qp_t10_ua_one(v) AS v FROM qp_t10_ua_t1
             UNION ALL SELECT id, qp_t10_ua_two(v)::varchar AS v FROM qp_t10_ua_t2;
         CREATE VIEW qp_t10_ua_distinct AS
             SELECT DISTINCT id, qp_t10_ua_one(v) AS v FROM qp_t10_ua_t1
             UNION ALL SELECT id, qp_t10_ua_two(v) FROM qp_t10_ua_t2;
         GRANT SELECT ON qp_t10_ua_union_all, qp_t10_ua_union,
             qp_t10_ua_order, qp_t10_ua_coerced, qp_t10_ua_distinct
             TO qp_t10_ua_app;
         GRANT SELECT ON qp_t10_ua_t1, qp_t10_ua_t2 TO qp_t10_ua_app;",
    )]);
    allowed(&db, "qp_t10_ua_app", "SELECT id FROM qp_t10_ua_union_all");
    denied_with(
        &db,
        "qp_t10_ua_app",
        "SELECT v FROM qp_t10_ua_union_all",
        "permission denied for function qp_t10_ua_one",
    );
    denied_with(
        &db,
        "qp_t10_ua_app",
        "SELECT id FROM qp_t10_ua_union",
        "permission denied for function qp_t10_ua_one",
    );
    denied_with(
        &db,
        "qp_t10_ua_app",
        "SELECT id FROM qp_t10_ua_order",
        "permission denied for function qp_t10_ua_one",
    );
    denied_with(
        &db,
        "qp_t10_ua_app",
        "SELECT id FROM qp_t10_ua_distinct",
        "permission denied for function qp_t10_ua_one",
    );
    // The coerced arm's type differs from the tree's: no pull-up.
    denied_with(
        &db,
        "qp_t10_ua_app",
        "SELECT id FROM qp_t10_ua_coerced",
        "permission denied for function qp_t10_ua_one",
    );
    // The same rule for a FROM subquery.
    allowed(
        &db,
        "qp_t10_ua_app",
        "SELECT id FROM (SELECT id, qp_t10_ua_one(v) AS v FROM qp_t10_ua_t1
                         UNION ALL
                         SELECT id, qp_t10_ua_two(v) FROM qp_t10_ua_t2) s",
    );
}

#[test]
fn a_stored_expressions_callees_are_its_pg_depend_oids_not_re_resolved_names() {
    // A stored expression's names resolved under the creation `search_path`
    // and `pg_depend` keeps the OIDs. Resolving them again under the running
    // role's path may pick another schema's function; that must not make the
    // created one look unused. Verified on PostgreSQL 18.6: the INSERT is
    // denied for `old_ns.guard`.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_oid_app;
         CREATE SCHEMA qp_t10_oid_old;
         CREATE SCHEMA qp_t10_oid_new;
         CREATE FUNCTION qp_t10_oid_old.guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t10_oid_new.guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         SET search_path = qp_t10_oid_old, public;
         CREATE TABLE public.qp_t10_oid_t (x int CHECK (guard(x)));
         RESET search_path;
         REVOKE EXECUTE ON FUNCTION qp_t10_oid_old.guard(int) FROM PUBLIC;
         GRANT USAGE ON SCHEMA qp_t10_oid_new TO qp_t10_oid_app;
         GRANT INSERT ON public.qp_t10_oid_t TO qp_t10_oid_app;
         ALTER ROLE qp_t10_oid_app SET search_path = qp_t10_oid_new, public;",
    )]);
    denied_with(
        &db,
        "qp_t10_oid_app",
        "INSERT INTO qp_t10_oid_t VALUES (1)",
        "permission denied for function guard",
    );
    // The reverse: the created function is granted, the other schema's
    // same-named one isn't — the check must not require the latter.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t10_oid2_app;
         CREATE SCHEMA qp_t10_oid2_old;
         CREATE SCHEMA qp_t10_oid2_new;
         CREATE FUNCTION qp_t10_oid2_old.guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t10_oid2_new.guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         SET search_path = qp_t10_oid2_old, public;
         CREATE TABLE public.qp_t10_oid2_t (x int CHECK (guard(x)));
         RESET search_path;
         REVOKE EXECUTE ON FUNCTION qp_t10_oid2_new.guard(int) FROM PUBLIC;
         GRANT USAGE ON SCHEMA qp_t10_oid2_new TO qp_t10_oid2_app;
         GRANT INSERT ON public.qp_t10_oid2_t TO qp_t10_oid2_app;
         ALTER ROLE qp_t10_oid2_app SET search_path = qp_t10_oid2_new, public;",
    )]);
    allowed(
        &db,
        "qp_t10_oid2_app",
        "INSERT INTO qp_t10_oid2_t VALUES (1)",
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Sixth round (T11): residuals the previous round missed
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn tablesample_parameters_run_where_pg_plans_and_evaluates_them() {
    // `set_tablesample_rel_size` estimates the sampling percentage
    // (`estimate_expression_value`): an IMMUTABLE or STABLE call over
    // constants there runs while planning, so its body is checked — in an
    // EXPLAIN without ANALYZE too. `REPEATABLE` is no part of the estimate:
    // only `eval_const_expressions` touches it (an IMMUTABLE call over
    // constants). Execution evaluates both. Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t11_ts_app;
         CREATE TABLE qp_t11_ts_hidden (x int);
         CREATE TABLE qp_t11_ts_t (id int);
         CREATE FUNCTION qp_t11_ts_read() RETURNS float4 LANGUAGE sql STABLE
             AS $$ SELECT max(x)::float4 FROM qp_t11_ts_hidden $$;
         CREATE FUNCTION qp_t11_ts_read_imm() RETURNS float4 LANGUAGE sql IMMUTABLE
             AS $$ SELECT max(x)::float4 FROM qp_t11_ts_hidden $$;
         CREATE FUNCTION qp_t11_ts_read_vol() RETURNS float4 LANGUAGE sql VOLATILE
             AS $$ SELECT max(x)::float4 FROM qp_t11_ts_hidden $$;
         CREATE FUNCTION qp_t11_ts_plain() RETURNS float4 LANGUAGE sql STABLE
             AS $$ SELECT 50.0::float4 $$;
         GRANT SELECT ON qp_t11_ts_t TO qp_t11_ts_app;
         GRANT EXECUTE ON FUNCTION qp_t11_ts_read(), qp_t11_ts_read_imm(),
             qp_t11_ts_read_vol(), qp_t11_ts_plain() TO qp_t11_ts_app;",
    )]);
    // The sampling percentage is estimated: a STABLE body runs, an
    // IMMUTABLE one too (everywhere).
    denied_with(
        &db,
        "qp_t11_ts_app",
        "EXPLAIN SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (qp_t11_ts_read())",
        "permission denied for table qp_t11_ts_hidden",
    );
    denied_with(
        &db,
        "qp_t11_ts_app",
        "EXPLAIN SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (qp_t11_ts_read_imm())",
        "permission denied for table qp_t11_ts_hidden",
    );
    // A VOLATILE call is not folded while planning.
    allowed(
        &db,
        "qp_t11_ts_app",
        "EXPLAIN SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (qp_t11_ts_read_vol())",
    );
    // REPEATABLE is const-folded only: an IMMUTABLE call runs, a STABLE
    // one waits for execution.
    denied_with(
        &db,
        "qp_t11_ts_app",
        "EXPLAIN SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (50) \
         REPEATABLE (qp_t11_ts_read_imm())",
        "permission denied for table qp_t11_ts_hidden",
    );
    allowed(
        &db,
        "qp_t11_ts_app",
        "EXPLAIN SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (50) \
         REPEATABLE (qp_t11_ts_read())",
    );
    allowed(
        &db,
        "qp_t11_ts_app",
        "EXPLAIN SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (50) \
         REPEATABLE (qp_t11_ts_read_vol())",
    );
    // Execution evaluates both arguments, whatever their volatility.
    denied_with(
        &db,
        "qp_t11_ts_app",
        "SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (qp_t11_ts_read_vol())",
        "permission denied for table qp_t11_ts_hidden",
    );
    denied_with(
        &db,
        "qp_t11_ts_app",
        "SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (50) \
         REPEATABLE (qp_t11_ts_read())",
        "permission denied for table qp_t11_ts_hidden",
    );
    // A parameter whose body reads nothing forbidden is fine.
    allowed(
        &db,
        "qp_t11_ts_app",
        "SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (qp_t11_ts_plain())",
    );
    allowed(
        &db,
        "qp_t11_ts_app",
        "EXPLAIN SELECT * FROM qp_t11_ts_t TABLESAMPLE SYSTEM (qp_t11_ts_plain())",
    );
}

#[test]
fn a_pulled_up_outputs_constant_is_folded_before_the_strict_null_shortcut() {
    // `pull_up_simple_subquery` substitutes a simple subquery's output
    // expression for the reference; `eval_const_expressions` then folds a
    // constant call over it *before* `evaluate_function` applies a strict
    // call's NULL shortcut — so `mod(NULL, f(col))` runs `f` when `col` is
    // certainly the constant `1`. The same holds for a view
    // (`ApplyRetrieveRule`) and an inlined CTE. A subquery not pulled up
    // (LIMIT, OFFSET, MATERIALIZED) keeps the column a Var, and `f` never
    // runs. Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t11_pu_app;
         CREATE TABLE qp_t11_pu_hidden (x int);
         CREATE TABLE qp_t11_pu_t (id int);
         CREATE VIEW qp_t11_pu_v AS SELECT 1 AS col;
         CREATE VIEW qp_t11_pu_lim AS SELECT 1 AS col LIMIT 1;
         CREATE FUNCTION qp_t11_pu_read(v int) RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT max(x)::int + $1 FROM qp_t11_pu_hidden $$;
         CREATE FUNCTION qp_t11_pu_leak() RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT max(x)::int FROM qp_t11_pu_hidden $$;
         CREATE FUNCTION qp_t11_pu_outer(v int) RETURNS int LANGUAGE sql STABLE
             AS $$ SELECT $1 + qp_t11_pu_leak() $$;
         GRANT EXECUTE ON FUNCTION qp_t11_pu_read(int), qp_t11_pu_leak(),
             qp_t11_pu_outer(int) TO qp_t11_pu_app;
         GRANT SELECT ON qp_t11_pu_t, qp_t11_pu_v, qp_t11_pu_lim TO qp_t11_pu_app;",
    )]);
    for sql in [
        "SELECT mod(NULL, qp_t11_pu_read(col)) FROM (SELECT 1 AS col) s",
        "EXPLAIN SELECT mod(NULL, qp_t11_pu_read(col)) FROM (SELECT 1 AS col) s",
        "SELECT mod(NULL, qp_t11_pu_read(col)) FROM qp_t11_pu_v",
        "WITH s AS (SELECT 1 AS col) SELECT mod(NULL, qp_t11_pu_read(col)) FROM s",
    ] {
        denied_with(
            &db,
            "qp_t11_pu_app",
            sql,
            "permission denied for table qp_t11_pu_hidden",
        );
    }
    // Not pulled up: the column stays a Var, and the strict call is NULL
    // before its argument is evaluated.
    for sql in [
        "SELECT mod(NULL, qp_t11_pu_read(col)) FROM (SELECT 1 AS col LIMIT 1) s",
        "SELECT mod(NULL, qp_t11_pu_read(col)) FROM (SELECT 1 AS col OFFSET 0) s",
        "SELECT mod(NULL, qp_t11_pu_read(col)) FROM qp_t11_pu_lim",
        "WITH s AS MATERIALIZED (SELECT 1 AS col)
         SELECT mod(NULL, qp_t11_pu_read(col)) FROM s",
        // A real table column is no constant, pulled up or not.
        "SELECT mod(NULL, qp_t11_pu_read(id)) FROM qp_t11_pu_t",
        "SELECT mod(NULL, qp_t11_pu_read(col)) FROM (SELECT id AS col FROM qp_t11_pu_t) s",
    ] {
        allowed(&db, "qp_t11_pu_app", sql);
    }
    // A call the planner only *inlines* is simplified before the strict
    // call is dropped: its body's constant calls still run, whatever its
    // argument (`inline_function`, then `eval_const_expressions`).
    for sql in [
        "SELECT mod(NULL, qp_t11_pu_outer(col)) FROM (SELECT 1 AS col) s",
        "EXPLAIN SELECT mod(NULL, qp_t11_pu_outer(col)) FROM (SELECT 1 AS col) s",
        "SELECT mod(NULL, qp_t11_pu_outer(id)) FROM qp_t11_pu_t",
        "EXPLAIN SELECT mod(NULL, qp_t11_pu_outer(id)) FROM qp_t11_pu_t",
    ] {
        denied_with(
            &db,
            "qp_t11_pu_app",
            sql,
            "permission denied for table qp_t11_pu_hidden",
        );
    }
}

#[test]
fn operator_functions_are_checked_like_function_calls() {
    // An operator call is a call of its implementation function: the
    // executor's `ExecInitFunc` checks its EXECUTE (a folded call's too, as
    // `evaluate_expr` initializes it), `evaluate_function` runs an IMMUTABLE
    // one over constant operands while planning, and a STRICT one with a
    // certainly NULL operand becomes NULL before either — but its operands
    // are simplified first. Verified on PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t11_op_app;
         CREATE TABLE qp_t11_op_hidden (x int);
         CREATE TABLE qp_t11_op_t (id int);
         CREATE FUNCTION qp_t11_op_fn(int, int) RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT $1 + $2 $$;
         CREATE FUNCTION qp_t11_op_read(int, int) RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT $1 + $2 + (SELECT max(x)::int FROM qp_t11_op_hidden) $$;
         CREATE FUNCTION qp_t11_op_stable(int, int) RETURNS int LANGUAGE sql STABLE
             AS $$ SELECT $1 + $2 + (SELECT max(x)::int FROM qp_t11_op_hidden) $$;
         CREATE FUNCTION qp_t11_op_strict(int, int) RETURNS int LANGUAGE sql IMMUTABLE STRICT
             AS $$ SELECT $1 + $2 + (SELECT max(x)::int FROM qp_t11_op_hidden) $$;
         CREATE FUNCTION qp_t11_op_strict_plain(int, int) RETURNS int LANGUAGE sql IMMUTABLE STRICT
             AS $$ SELECT $1 + $2 $$;
         CREATE FUNCTION qp_t11_op_leak() RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT max(x)::int FROM qp_t11_op_hidden $$;
         CREATE OPERATOR ## (LEFTARG = int, RIGHTARG = int, FUNCTION = qp_t11_op_fn);
         CREATE OPERATOR @@ (LEFTARG = int, RIGHTARG = int, FUNCTION = qp_t11_op_read);
         CREATE OPERATOR !! (LEFTARG = int, RIGHTARG = int, FUNCTION = qp_t11_op_stable);
         CREATE OPERATOR ?# (LEFTARG = int, RIGHTARG = int, FUNCTION = qp_t11_op_strict);
         CREATE OPERATOR %# (LEFTARG = int, RIGHTARG = int, FUNCTION = qp_t11_op_strict_plain);
         CREATE TABLE qp_t11_op_chk (x int CHECK (x ## 2 > 0));
         CREATE TABLE qp_t11_op_def (y int DEFAULT (1 @@ 2));
         REVOKE EXECUTE ON FUNCTION qp_t11_op_fn(int, int),
             qp_t11_op_strict_plain(int, int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t11_op_read(int, int), qp_t11_op_stable(int, int),
             qp_t11_op_strict(int, int), qp_t11_op_leak() TO qp_t11_op_app;
         GRANT SELECT ON qp_t11_op_t TO qp_t11_op_app;
         GRANT INSERT ON qp_t11_op_chk, qp_t11_op_def TO qp_t11_op_app;",
    )]);
    // The operator's function needs EXECUTE: folded or not, and through a
    // strict call's argument simplification too.
    for sql in [
        "SELECT 1 ## 2",
        "EXPLAIN SELECT 1 ## 2",
        "SELECT id ## 2 FROM qp_t11_op_t",
        "EXPLAIN SELECT id ## 2 FROM qp_t11_op_t",
        "SELECT mod(NULL::int, 1 ## 2)",
        "INSERT INTO qp_t11_op_chk VALUES (3)",
    ] {
        denied_with(
            &db,
            "qp_t11_op_app",
            sql,
            "permission denied for function qp_t11_op_fn",
        );
    }
    // A constant call runs its body while planning, or at executor start
    // when the operator is not folded; neither reaches a forbidden table
    // for a column operand.
    for sql in [
        "SELECT 1 @@ 2",
        "EXPLAIN SELECT 1 @@ 2",
        "SELECT mod(NULL::int, 1 @@ 2)",
        "EXPLAIN SELECT * FROM qp_t11_op_t WHERE id = (1 !! 2)",
        "EXPLAIN SELECT * FROM qp_t11_op_t WHERE id = (1 @@ 2)",
        "INSERT INTO qp_t11_op_def DEFAULT VALUES",
        "EXPLAIN INSERT INTO qp_t11_op_def DEFAULT VALUES",
    ] {
        denied_with(
            &db,
            "qp_t11_op_app",
            sql,
            "permission denied for table qp_t11_op_hidden",
        );
    }
    // A STRICT operator with a certainly NULL operand is NULL before its
    // function runs — but its operands are simplified first, so a folded
    // call among them still runs.
    allowed(&db, "qp_t11_op_app", "SELECT NULL::int ?# 2");
    allowed(&db, "qp_t11_op_app", "EXPLAIN SELECT NULL::int ?# 2");
    // Not even EXECUTE is checked for the STRICT call replaced by NULL.
    allowed(&db, "qp_t11_op_app", "SELECT NULL::int %# 2");
    allowed(&db, "qp_t11_op_app", "EXPLAIN SELECT NULL::int %# 2");
    for sql in [
        "SELECT 1 %# 2",
        "EXPLAIN SELECT 1 %# 2",
        "SELECT id %# 2 FROM qp_t11_op_t",
    ] {
        denied_with(
            &db,
            "qp_t11_op_app",
            sql,
            "permission denied for function qp_t11_op_strict_plain",
        );
    }
    denied_with(
        &db,
        "qp_t11_op_app",
        "SELECT NULL::int ?# (1 + qp_t11_op_leak())",
        "permission denied for table qp_t11_op_hidden",
    );
    denied_with(
        &db,
        "qp_t11_op_app",
        "EXPLAIN SELECT NULL::int ?# (1 + qp_t11_op_leak())",
        "permission denied for table qp_t11_op_hidden",
    );
}

// ---------------------------------------------------------------------------
// Nested set operations restore the output-tracking state
// ---------------------------------------------------------------------------

#[test]
fn a_nested_union_all_tree_leaves_output_tracking_balanced() {
    // A set-operation node nested in a simple `UNION ALL` tree shares the
    // tree's output group and never disables tracking for its arms, so
    // leaving it must not re-enable tracking either. An unbalanced counter
    // underflows (a panic in debug builds; in release it wraps and turns
    // the pruning of unused view outputs off for every later analysis on
    // the thread).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_nua_app;
         CREATE TABLE qp_nua_t (id int, v text);
         CREATE FUNCTION qp_nua_mask(text) RETURNS text LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN left($1, 1) || '***'; END $$;
         REVOKE EXECUTE ON FUNCTION qp_nua_mask(text) FROM PUBLIC;
         CREATE VIEW qp_nua_v AS SELECT id, qp_nua_mask(v) AS v FROM qp_nua_t;
         GRANT SELECT ON qp_nua_t, qp_nua_v TO qp_nua_app;",
    )]);
    for sql in [
        "SELECT id FROM qp_nua_t UNION ALL (SELECT id FROM qp_nua_t UNION ALL SELECT id FROM qp_nua_t)",
        "SELECT s.id FROM (SELECT id FROM qp_nua_t UNION ALL (SELECT id FROM qp_nua_t UNION ALL SELECT id FROM qp_nua_t)) s",
        "(SELECT id FROM qp_nua_t UNION ALL SELECT id FROM qp_nua_t) UNION ALL (SELECT id FROM qp_nua_t UNION ALL SELECT id FROM qp_nua_t)",
    ] {
        allowed(&db, "qp_nua_app", sql);
        // Tracking still works afterwards: the unused masked output of the
        // view is dropped.
        allowed(&db, "qp_nua_app", "SELECT id FROM qp_nua_v");
    }
    denied_with(
        &db,
        "qp_nua_app",
        "SELECT v FROM qp_nua_v",
        "permission denied for function qp_nua_mask",
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Seventh round (T12): what an EXPLAIN without ANALYZE initializes per row,
// and the operator functions of ANY / IN / NULLIF
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn explain_without_analyze_initializes_no_per_row_expression() {
    // ExecConstraints, index insertion, ExecComputeStoredGenerated,
    // ExecFindPartition and a trigger's WHEN condition evaluate per row:
    // ExplainOnePlan starts the executor and never runs it, so none of them
    // is initialized and none of their routines' EXECUTE is checked. The
    // statement that runs them checks them. A column default, a domain
    // coercion the rewriter puts in the target list (the INSERT values, the
    // UPDATE SET values, a domain default) and a read of a virtual generated
    // column are plan expressions: ExecInitExpr initializes them, and the
    // planner folds an IMMUTABLE default over constants (`evaluate_function`)
    // — its EXECUTE and the body the fold runs are checked. Verified on
    // PostgreSQL 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t12_ex_app;
         CREATE TABLE qp_t12_ex_secret (x int);
         CREATE FUNCTION qp_t12_ex_exec(int) RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT $1 $$;
         CREATE FUNCTION qp_t12_ex_exec0() RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT 1 $$;
         CREATE FUNCTION qp_t12_ex_body(int) RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT $1 + (SELECT max(x)::int FROM public.qp_t12_ex_secret) $$;
         CREATE FUNCTION qp_t12_ex_body0() RETURNS int LANGUAGE sql IMMUTABLE
             AS $$ SELECT (SELECT max(x)::int FROM public.qp_t12_ex_secret) $$;
         REVOKE EXECUTE ON FUNCTION qp_t12_ex_exec(int), qp_t12_ex_exec0() FROM PUBLIC;
         CREATE TABLE qp_t12_ex_check (id int, x int CHECK (qp_t12_ex_exec(x) > 0));
         CREATE TABLE qp_t12_ex_check_body (id int, x int CHECK (qp_t12_ex_body(x) > 0));
         CREATE TABLE qp_t12_ex_check_const (id int, x int CHECK (qp_t12_ex_exec0() > 0));
         CREATE TABLE qp_t12_ex_idx (id int, x int);
         CREATE INDEX qp_t12_ex_idx_expr ON qp_t12_ex_idx ((qp_t12_ex_exec(x)));
         CREATE TABLE qp_t12_ex_idx_body (id int, x int);
         CREATE INDEX qp_t12_ex_idx_body_expr ON qp_t12_ex_idx_body ((qp_t12_ex_body(x)));
         CREATE TABLE qp_t12_ex_pred (id int, x int);
         CREATE INDEX qp_t12_ex_pred_i ON qp_t12_ex_pred (id)
             WHERE qp_t12_ex_exec(x) > 0;
         CREATE TABLE qp_t12_ex_pred_body (id int, x int);
         CREATE INDEX qp_t12_ex_pred_body_i ON qp_t12_ex_pred_body (id)
             WHERE qp_t12_ex_body(x) > 0;
         CREATE TABLE qp_t12_ex_pred_const (id int, x int);
         CREATE INDEX qp_t12_ex_pred_const_i ON qp_t12_ex_pred_const (id)
             WHERE qp_t12_ex_exec0() > 0;
         CREATE TABLE qp_t12_ex_gen (id int, x int,
             g int GENERATED ALWAYS AS (qp_t12_ex_exec(x)) STORED);
         CREATE TABLE qp_t12_ex_gen_body (id int, x int,
             g int GENERATED ALWAYS AS (qp_t12_ex_body(x)) STORED);
         CREATE TABLE qp_t12_ex_def (id int, d int DEFAULT qp_t12_ex_exec0());
         CREATE TABLE qp_t12_ex_def_body (id int, d int DEFAULT qp_t12_ex_body0());
         CREATE TABLE qp_t12_ex_part (id int, x int)
             PARTITION BY RANGE (qp_t12_ex_exec(x));
         CREATE TABLE qp_t12_ex_part_p1 PARTITION OF qp_t12_ex_part
             FOR VALUES FROM (MINVALUE) TO (MAXVALUE);
         CREATE TABLE qp_t12_ex_part_body (id int, x int)
             PARTITION BY RANGE (qp_t12_ex_body(x));
         CREATE TABLE qp_t12_ex_part_body_p1 PARTITION OF qp_t12_ex_part_body
             FOR VALUES FROM (MINVALUE) TO (MAXVALUE);
         CREATE FUNCTION qp_t12_ex_trig() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN RETURN NEW; END $$;
         GRANT EXECUTE ON FUNCTION qp_t12_ex_trig() TO qp_t12_ex_app;
         CREATE TABLE qp_t12_ex_trg (id int, x int);
         CREATE TRIGGER qp_t12_ex_trg_when BEFORE INSERT ON qp_t12_ex_trg
             FOR EACH ROW WHEN (qp_t12_ex_exec(NEW.x) > 0)
             EXECUTE FUNCTION qp_t12_ex_trig();
         CREATE TABLE qp_t12_ex_trg_body (id int, x int);
         CREATE TRIGGER qp_t12_ex_trg_body_when BEFORE INSERT ON qp_t12_ex_trg_body
             FOR EACH ROW WHEN (qp_t12_ex_body(NEW.x) > 0)
             EXECUTE FUNCTION qp_t12_ex_trig();
         CREATE DOMAIN qp_t12_ex_dom AS int CHECK (qp_t12_ex_exec(VALUE) > 0);
         CREATE TABLE qp_t12_ex_dom_t (id int, a qp_t12_ex_dom);
         CREATE TABLE qp_t12_ex_dom_check (id int, x int CHECK (x::qp_t12_ex_dom > 0));
         CREATE TABLE qp_t12_ex_dom_idx (id int, x int);
         CREATE INDEX qp_t12_ex_dom_idx_i ON qp_t12_ex_dom_idx
             (((x::qp_t12_ex_dom)::int));
         CREATE TABLE qp_t12_ex_dom_pred (id int, x int);
         CREATE INDEX qp_t12_ex_dom_pred_i ON qp_t12_ex_dom_pred (id)
             WHERE (x::qp_t12_ex_dom) > 0;
         CREATE TABLE qp_t12_ex_dom_defcast (id int,
             d int DEFAULT ((1::qp_t12_ex_dom)::int));
         CREATE TABLE qp_t12_ex_dom_gen (id int, x int,
             g qp_t12_ex_dom GENERATED ALWAYS AS (x) STORED);
         CREATE DOMAIN qp_t12_ex_domdef AS int DEFAULT qp_t12_ex_exec0();
         CREATE TABLE qp_t12_ex_domdef_t (id int, a qp_t12_ex_domdef);
         GRANT SELECT, INSERT, UPDATE ON qp_t12_ex_check, qp_t12_ex_check_body,
             qp_t12_ex_check_const, qp_t12_ex_idx, qp_t12_ex_idx_body,
             qp_t12_ex_pred, qp_t12_ex_pred_body, qp_t12_ex_pred_const,
             qp_t12_ex_gen, qp_t12_ex_gen_body, qp_t12_ex_def,
             qp_t12_ex_def_body, qp_t12_ex_part, qp_t12_ex_part_body,
             qp_t12_ex_trg, qp_t12_ex_trg_body, qp_t12_ex_dom_t,
             qp_t12_ex_dom_gen, qp_t12_ex_domdef_t, qp_t12_ex_dom_check,
             qp_t12_ex_dom_idx, qp_t12_ex_dom_pred, qp_t12_ex_dom_defcast,
             qp_t12_ex_secret TO qp_t12_ex_app;
         REVOKE SELECT ON qp_t12_ex_secret FROM qp_t12_ex_app;",
    )]);
    // Per-row expressions: EXPLAIN initializes none of them.
    for sql in [
        "EXPLAIN INSERT INTO qp_t12_ex_check VALUES (1, 2)",
        "EXPLAIN UPDATE qp_t12_ex_check SET x = 2",
        "EXPLAIN INSERT INTO qp_t12_ex_idx VALUES (1, 2)",
        "EXPLAIN UPDATE qp_t12_ex_idx SET x = 2",
        "EXPLAIN INSERT INTO qp_t12_ex_pred VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_gen VALUES (1, 2)",
        "EXPLAIN UPDATE qp_t12_ex_gen SET x = 2",
        "EXPLAIN INSERT INTO qp_t12_ex_part VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_trg VALUES (1, 2)",
        // A constant-only CHECK and index predicate are not part of the
        // plan either: the planner never folds them.
        "EXPLAIN INSERT INTO qp_t12_ex_check_const VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_pred_const VALUES (1, 2)",
        // With EXECUTE granted, the per-row body is not run either.
        "EXPLAIN INSERT INTO qp_t12_ex_check_body VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_idx_body VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_pred_body VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_gen_body VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_part_body VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_trg_body VALUES (1, 2)",
        // A stored generated column's domain coercion is part of its
        // generation expression, evaluated per row.
        "EXPLAIN INSERT INTO qp_t12_ex_dom_gen VALUES (1, 2)",
        "EXPLAIN UPDATE qp_t12_ex_dom_gen SET x = 2",
        // So is a cast to a domain inside a CHECK or an index expression.
        "EXPLAIN INSERT INTO qp_t12_ex_dom_check VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_dom_idx VALUES (1, 2)",
        "EXPLAIN INSERT INTO qp_t12_ex_dom_pred VALUES (1, 2)",
    ] {
        allowed(&db, "qp_t12_ex_app", sql);
    }
    // Plan expressions the rewriter adds: checked by EXPLAIN. A domain's
    // constraint expressions are prepared once per backend, under the user
    // that first evaluates them (`DomainConstraintRef`), so these precede
    // the statements that run below (which let the oracle seed rows as the
    // superuser and would prepare them under that user).
    for (sql, prefix) in [
        (
            "EXPLAIN INSERT INTO qp_t12_ex_def (id) VALUES (1)",
            "permission denied for function qp_t12_ex_exec0",
        ),
        (
            "EXPLAIN UPDATE qp_t12_ex_def SET d = DEFAULT",
            "permission denied for function qp_t12_ex_exec0",
        ),
        (
            "EXPLAIN INSERT INTO qp_t12_ex_def_body (id) VALUES (1)",
            "permission denied for table qp_t12_ex_secret",
        ),
        (
            "EXPLAIN INSERT INTO qp_t12_ex_dom_t VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "EXPLAIN INSERT INTO qp_t12_ex_dom_t (id) VALUES (1)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "EXPLAIN UPDATE qp_t12_ex_dom_t SET a = 2",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "EXPLAIN INSERT INTO qp_t12_ex_domdef_t (id) VALUES (1)",
            "permission denied for function qp_t12_ex_exec0",
        ),
        (
            "EXPLAIN INSERT INTO qp_t12_ex_dom_defcast (id) VALUES (1)",
            "permission denied for function qp_t12_ex_exec",
        ),
    ] {
        denied_with(&db, "qp_t12_ex_app", sql, prefix);
    }
    // The same writes run: every per-row expression is checked then.
    for (sql, prefix) in [
        (
            "INSERT INTO qp_t12_ex_check VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "UPDATE qp_t12_ex_check SET x = 2",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "INSERT INTO qp_t12_ex_idx VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "INSERT INTO qp_t12_ex_gen VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "INSERT INTO qp_t12_ex_part VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "INSERT INTO qp_t12_ex_trg VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "INSERT INTO qp_t12_ex_check_body VALUES (1, 2)",
            "permission denied for table qp_t12_ex_secret",
        ),
        (
            "INSERT INTO qp_t12_ex_gen_body VALUES (1, 2)",
            "permission denied for table qp_t12_ex_secret",
        ),
        (
            "INSERT INTO qp_t12_ex_dom_check VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "INSERT INTO qp_t12_ex_dom_idx VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "INSERT INTO qp_t12_ex_dom_pred VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
        (
            "EXPLAIN ANALYZE INSERT INTO qp_t12_ex_check VALUES (1, 2)",
            "permission denied for function qp_t12_ex_exec",
        ),
    ] {
        denied_with(&db, "qp_t12_ex_app", sql, prefix);
    }
    // No coercion, no check.
    for sql in [
        "EXPLAIN SELECT a FROM qp_t12_ex_dom_t",
        "EXPLAIN UPDATE qp_t12_ex_dom_t SET a = a",
    ] {
        allowed(&db, "qp_t12_ex_app", sql);
    }
}

#[test]
fn operator_functions_in_any_in_and_nullif_are_checked_like_calls() {
    // `x = ANY(ARRAY[...])` (which `IN` becomes when its list is constant)
    // calls its comparison function per element; `NULLIF` calls its `=`
    // function once. With a column operand they are plan expressions:
    // ExecInitExpr / ExecInitScalarArrayOp initialize the function's EXECUTE
    // — an EXPLAIN without ANALYZE too — but evaluate no row. Constant
    // operands are folded by `eval_const_expressions` (`ece_evaluate_expr`
    // on the ScalarArrayOpExpr, a NullIfExpr with no NULL operand), running
    // the function and its body while planning; a NULLIF with a certainly
    // NULL operand is NULL without initializing it. Verified on PostgreSQL
    // 18.6.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t12_op_app;
         CREATE TABLE qp_t12_op_secret (x int);
         CREATE TYPE qp_t12_op_ct AS (a int);
         CREATE FUNCTION qp_t12_op_exec(qp_t12_op_ct, qp_t12_op_ct)
             RETURNS boolean LANGUAGE sql IMMUTABLE AS $$ SELECT true $$;
         REVOKE EXECUTE ON FUNCTION qp_t12_op_exec(qp_t12_op_ct, qp_t12_op_ct)
             FROM PUBLIC;
         CREATE OPERATOR = (LEFTARG = qp_t12_op_ct, RIGHTARG = qp_t12_op_ct,
             FUNCTION = qp_t12_op_exec);
         CREATE TABLE qp_t12_op_t (id int, d qp_t12_op_ct);
         CREATE TYPE qp_t12_op_ct2 AS (a int);
         CREATE FUNCTION qp_t12_op_body(qp_t12_op_ct2, qp_t12_op_ct2)
             RETURNS boolean LANGUAGE sql IMMUTABLE
             AS $$ SELECT (SELECT max(x)::int FROM qp_t12_op_secret) > 0 $$;
         GRANT EXECUTE ON FUNCTION qp_t12_op_body(qp_t12_op_ct2, qp_t12_op_ct2)
             TO qp_t12_op_app;
         CREATE OPERATOR = (LEFTARG = qp_t12_op_ct2, RIGHTARG = qp_t12_op_ct2,
             FUNCTION = qp_t12_op_body);
         CREATE TABLE qp_t12_op_t2 (id int, d qp_t12_op_ct2);
         GRANT SELECT, INSERT ON qp_t12_op_t, qp_t12_op_t2, qp_t12_op_secret
             TO qp_t12_op_app;
         REVOKE SELECT ON qp_t12_op_secret FROM qp_t12_op_app;",
    )]);
    // The operator's function needs EXECUTE: as a plan expression, and in
    // the fold of constant operands.
    for sql in [
        "EXPLAIN SELECT d = ANY(ARRAY[ROW(1)::qp_t12_op_ct]) FROM qp_t12_op_t",
        "EXPLAIN SELECT d IN (ROW(1)::qp_t12_op_ct) FROM qp_t12_op_t",
        "EXPLAIN SELECT d IN (ROW(1)::qp_t12_op_ct, ROW(2)::qp_t12_op_ct)
             FROM qp_t12_op_t",
        "EXPLAIN SELECT NULLIF(d, ROW(1)::qp_t12_op_ct) FROM qp_t12_op_t",
        "EXPLAIN SELECT ROW(1)::qp_t12_op_ct = ANY(ARRAY[ROW(1)::qp_t12_op_ct])",
        "EXPLAIN SELECT ROW(1)::qp_t12_op_ct IN (ROW(1)::qp_t12_op_ct,
             ROW(2)::qp_t12_op_ct)",
        "EXPLAIN SELECT NULLIF(ROW(1)::qp_t12_op_ct, ROW(2)::qp_t12_op_ct)",
    ] {
        denied_with(
            &db,
            "qp_t12_op_app",
            sql,
            "permission denied for function qp_t12_op_exec",
        );
    }
    // A NULL operand keeps a NULLIF from initializing the function at all.
    for sql in [
        "EXPLAIN SELECT NULLIF(d, NULL) FROM qp_t12_op_t",
        "EXPLAIN SELECT NULLIF(NULL, d) FROM qp_t12_op_t",
        "EXPLAIN SELECT NULLIF(NULL::qp_t12_op_ct, ROW(1)::qp_t12_op_ct)",
    ] {
        allowed(&db, "qp_t12_op_app", sql);
    }
    // A granted function whose body reads a forbidden table: a column
    // operand leaves it to the row's evaluation (EXPLAIN evaluates none),
    // constant operands run it while planning.
    for sql in [
        "EXPLAIN SELECT d = ANY(ARRAY[ROW(1)::qp_t12_op_ct2]) FROM qp_t12_op_t2",
        "EXPLAIN SELECT d IN (ROW(1)::qp_t12_op_ct2) FROM qp_t12_op_t2",
        "EXPLAIN SELECT d IN (ROW(1)::qp_t12_op_ct2, ROW(2)::qp_t12_op_ct2)
             FROM qp_t12_op_t2",
        "EXPLAIN SELECT NULLIF(d, ROW(1)::qp_t12_op_ct2) FROM qp_t12_op_t2",
        "EXPLAIN SELECT NULLIF(d, NULL) FROM qp_t12_op_t2",
    ] {
        allowed(&db, "qp_t12_op_app", sql);
    }
    for sql in [
        "SELECT d = ANY(ARRAY[ROW(1)::qp_t12_op_ct2]) FROM qp_t12_op_t2",
        "SELECT d IN (ROW(1)::qp_t12_op_ct2) FROM qp_t12_op_t2",
        "SELECT d IN (ROW(1)::qp_t12_op_ct2, ROW(2)::qp_t12_op_ct2) FROM qp_t12_op_t2",
        "SELECT NULLIF(d, ROW(1)::qp_t12_op_ct2) FROM qp_t12_op_t2",
        "EXPLAIN SELECT ROW(1)::qp_t12_op_ct2 = ANY(ARRAY[ROW(1)::qp_t12_op_ct2])",
        "EXPLAIN SELECT ROW(1)::qp_t12_op_ct2 IN (ROW(1)::qp_t12_op_ct2,
             ROW(2)::qp_t12_op_ct2)",
        "EXPLAIN SELECT NULLIF(ROW(1)::qp_t12_op_ct2, ROW(2)::qp_t12_op_ct2)",
    ] {
        denied_with(
            &db,
            "qp_t12_op_app",
            sql,
            "permission denied for table qp_t12_op_secret",
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Eighth round (T13): row-level security policy expressions, and a
// single-item IN over row constructors
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn row_security_policy_expressions_are_checked() {
    // When row security applies (the table enabled it, the role is neither
    // its owner nor BYPASSRLS / superuser), PG adds the applicable
    // policies' USING / WITH CHECK expressions to the query. Their
    // functions are initialized with the plan (`ExecInitFunc`, as the
    // invoking role), so their EXECUTE is checked — `EXPLAIN` without
    // `ANALYZE` included, since it starts the executor. Verified on
    // PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_chk_app;
         CREATE ROLE qp_t13_chk_other;
         CREATE TABLE qp_t13_chk_t (id int, owner_id int);
         GRANT SELECT ON qp_t13_chk_t TO qp_t13_chk_app, qp_t13_chk_other;
         ALTER TABLE qp_t13_chk_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_chk_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_chk_guard(int) FROM PUBLIC;
         CREATE POLICY qp_t13_chk_p ON qp_t13_chk_t FOR SELECT
             TO qp_t13_chk_app USING (qp_t13_chk_guard(owner_id));",
    )]);
    denied_with(
        &db,
        "qp_t13_chk_app",
        "SELECT * FROM qp_t13_chk_t",
        "permission denied for function qp_t13_chk_guard",
    );
    // EXPLAIN without ANALYZE starts the executor without running it.
    denied_with(
        &db,
        "qp_t13_chk_app",
        "EXPLAIN SELECT * FROM qp_t13_chk_t",
        "permission denied for function qp_t13_chk_guard",
    );
    // The policy is not addressed to this role: no policy applies, and the
    // default-deny `false` runs nothing.
    allowed(&db, "qp_t13_chk_other", "SELECT * FROM qp_t13_chk_t");

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_chk2_app;
         CREATE TABLE qp_t13_chk2_t (id int, owner_id int);
         GRANT SELECT ON qp_t13_chk2_t TO qp_t13_chk2_app;
         ALTER TABLE qp_t13_chk2_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_chk2_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_chk2_guard(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t13_chk2_guard(int) TO qp_t13_chk2_app;
         CREATE POLICY qp_t13_chk2_p ON qp_t13_chk2_t FOR SELECT
             USING (qp_t13_chk2_guard(owner_id));",
    )]);
    allowed(&db, "qp_t13_chk2_app", "SELECT * FROM qp_t13_chk2_t");
    allowed(
        &db,
        "qp_t13_chk2_app",
        "EXPLAIN SELECT * FROM qp_t13_chk2_t",
    );
}

#[test]
fn row_security_policy_folding_is_respected() {
    // `add_security_quals`: the permissive policies' USING clauses are
    // OR-ed, the restrictive ones added as separate AND-ed quals; with no
    // permissive clause the default-deny `false` is used and the
    // restrictive policies are never built. The planner's constant folding
    // prunes what certainly never runs: `true OR f()` and `false AND f()`
    // evaluate no `f`. Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_fold1_app;
         CREATE TABLE qp_t13_fold1_t (id int);
         GRANT SELECT ON qp_t13_fold1_t TO qp_t13_fold1_app;
         ALTER TABLE qp_t13_fold1_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_fold1_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_fold1_guard(int) FROM PUBLIC;
         CREATE POLICY qp_t13_fold1_any ON qp_t13_fold1_t FOR SELECT
             USING (true);
         CREATE POLICY qp_t13_fold1_guard ON qp_t13_fold1_t FOR SELECT
             USING (qp_t13_fold1_guard(id));",
    )]);
    allowed(&db, "qp_t13_fold1_app", "SELECT * FROM qp_t13_fold1_t");

    // A restrictive policy alone denies every row and is not built: its
    // function is never checked.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_fold2_app;
         CREATE TABLE qp_t13_fold2_t (id int);
         GRANT SELECT ON qp_t13_fold2_t TO qp_t13_fold2_app;
         ALTER TABLE qp_t13_fold2_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_fold2_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_fold2_guard(int) FROM PUBLIC;
         CREATE POLICY qp_t13_fold2_restrict ON qp_t13_fold2_t AS RESTRICTIVE
             FOR SELECT USING (qp_t13_fold2_guard(id));",
    )]);
    allowed(&db, "qp_t13_fold2_app", "SELECT * FROM qp_t13_fold2_t");

    // A restrictive `false` makes the AND constant, so the permissive
    // clause is dropped with it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_fold3_app;
         CREATE TABLE qp_t13_fold3_t (id int);
         GRANT SELECT ON qp_t13_fold3_t TO qp_t13_fold3_app;
         ALTER TABLE qp_t13_fold3_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_fold3_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_fold3_guard(int) FROM PUBLIC;
         CREATE POLICY qp_t13_fold3_restrict ON qp_t13_fold3_t AS RESTRICTIVE
             FOR SELECT USING (false);
         CREATE POLICY qp_t13_fold3_guard ON qp_t13_fold3_t FOR SELECT
             USING (qp_t13_fold3_guard(id));",
    )]);
    allowed(&db, "qp_t13_fold3_app", "SELECT * FROM qp_t13_fold3_t");

    // Without the constant both policies' functions are checked.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_fold4_app;
         CREATE TABLE qp_t13_fold4_t (id int);
         GRANT SELECT ON qp_t13_fold4_t TO qp_t13_fold4_app;
         ALTER TABLE qp_t13_fold4_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_fold4_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_fold4_guard(int) FROM PUBLIC;
         CREATE POLICY qp_t13_fold4_restrict ON qp_t13_fold4_t AS RESTRICTIVE
             FOR SELECT USING (qp_t13_fold4_guard(id));
         CREATE POLICY qp_t13_fold4_any ON qp_t13_fold4_t FOR SELECT
             USING (id = 1);",
    )]);
    denied_with(
        &db,
        "qp_t13_fold4_app",
        "SELECT * FROM qp_t13_fold4_t",
        "permission denied for function qp_t13_fold4_guard",
    );

    // A single policy expression folds like any stored expression's.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_fold5_app;
         CREATE TABLE qp_t13_fold5_t (id int);
         GRANT SELECT ON qp_t13_fold5_t TO qp_t13_fold5_app;
         ALTER TABLE qp_t13_fold5_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_fold5_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_fold5_guard(int) FROM PUBLIC;
         CREATE POLICY qp_t13_fold5_and ON qp_t13_fold5_t FOR SELECT
             USING (false AND qp_t13_fold5_guard(id));
         CREATE POLICY qp_t13_fold5_or ON qp_t13_fold5_t FOR SELECT
             USING (true OR qp_t13_fold5_guard(id));",
    )]);
    allowed(&db, "qp_t13_fold5_app", "SELECT * FROM qp_t13_fold5_t");
}

#[test]
fn row_security_policies_per_command() {
    // `get_row_security_policies`: a read of a non-target relation applies
    // its SELECT policies; `SELECT ... FOR UPDATE` adds the UPDATE USING
    // clauses; the target of INSERT / UPDATE / DELETE applies its command's
    // policies, plus the SELECT ones when the statement reads the table
    // (`WHERE`, `RETURNING`, `ON CONFLICT`). Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_cmd_app;
         CREATE TABLE qp_t13_cmd_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_t13_cmd_t TO qp_t13_cmd_app;
         CREATE UNIQUE INDEX qp_t13_cmd_t_id ON qp_t13_cmd_t (id);
         ALTER TABLE qp_t13_cmd_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_cmd_sel(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_cmd_ins(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_cmd_upd(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_cmd_del(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_cmd_sel(int), qp_t13_cmd_ins(int),
             qp_t13_cmd_upd(int), qp_t13_cmd_del(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t13_cmd_sel(int), qp_t13_cmd_ins(int),
             qp_t13_cmd_upd(int), qp_t13_cmd_del(int) TO qp_t13_cmd_app;
         CREATE POLICY qp_t13_cmd_sel ON qp_t13_cmd_t FOR SELECT
             USING (qp_t13_cmd_sel(id));
         CREATE POLICY qp_t13_cmd_ins ON qp_t13_cmd_t FOR INSERT
             WITH CHECK (qp_t13_cmd_ins(id));
         CREATE POLICY qp_t13_cmd_upd ON qp_t13_cmd_t FOR UPDATE
             USING (qp_t13_cmd_upd(id)) WITH CHECK (qp_t13_cmd_upd(id));
         CREATE POLICY qp_t13_cmd_del ON qp_t13_cmd_t FOR DELETE
             USING (qp_t13_cmd_del(id));",
    )]);
    // Only the command's own policies apply.
    allowed(&db, "qp_t13_cmd_app", "SELECT * FROM qp_t13_cmd_t");
    allowed(
        &db,
        "qp_t13_cmd_app",
        "INSERT INTO qp_t13_cmd_t VALUES (1, 1)",
    );
    allowed(&db, "qp_t13_cmd_app", "UPDATE qp_t13_cmd_t SET x = 1");
    allowed(&db, "qp_t13_cmd_app", "DELETE FROM qp_t13_cmd_t");
    for sql in [
        // RETURNING reads the target: the SELECT policies apply too.
        "INSERT INTO qp_t13_cmd_t VALUES (1, 1) RETURNING id",
        "INSERT INTO qp_t13_cmd_t VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET x = 1",
        // A WHERE reads the target.
        "UPDATE qp_t13_cmd_t SET x = 1 WHERE id = 1",
        "DELETE FROM qp_t13_cmd_t WHERE id = 1",
        // The UPDATE policies filter a locking read.
        "SELECT * FROM qp_t13_cmd_t FOR UPDATE",
    ] {
        allowed(&db, "qp_t13_cmd_app", sql);
    }

    // One role per revoked function pins which statements check it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_cmdr_nosel;
         CREATE ROLE qp_t13_cmdr_noins;
         CREATE ROLE qp_t13_cmdr_noupd;
         CREATE ROLE qp_t13_cmdr_nodel;
         CREATE TABLE qp_t13_cmdr_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_t13_cmdr_t
             TO qp_t13_cmdr_nosel, qp_t13_cmdr_noins,
                qp_t13_cmdr_noupd, qp_t13_cmdr_nodel;
         CREATE UNIQUE INDEX qp_t13_cmdr_t_id ON qp_t13_cmdr_t (id);
         ALTER TABLE qp_t13_cmdr_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_cmdr_sel(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_cmdr_ins(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_cmdr_upd(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_cmdr_del(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_cmdr_sel(int), qp_t13_cmdr_ins(int),
             qp_t13_cmdr_upd(int), qp_t13_cmdr_del(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t13_cmdr_ins(int), qp_t13_cmdr_upd(int),
             qp_t13_cmdr_del(int) TO qp_t13_cmdr_nosel;
         GRANT EXECUTE ON FUNCTION qp_t13_cmdr_sel(int), qp_t13_cmdr_upd(int),
             qp_t13_cmdr_del(int) TO qp_t13_cmdr_noins;
         GRANT EXECUTE ON FUNCTION qp_t13_cmdr_sel(int), qp_t13_cmdr_ins(int),
             qp_t13_cmdr_del(int) TO qp_t13_cmdr_noupd;
         GRANT EXECUTE ON FUNCTION qp_t13_cmdr_sel(int), qp_t13_cmdr_ins(int),
             qp_t13_cmdr_upd(int) TO qp_t13_cmdr_nodel;
         CREATE POLICY qp_t13_cmdr_sel ON qp_t13_cmdr_t FOR SELECT
             USING (qp_t13_cmdr_sel(id));
         CREATE POLICY qp_t13_cmdr_ins ON qp_t13_cmdr_t FOR INSERT
             WITH CHECK (qp_t13_cmdr_ins(id));
         CREATE POLICY qp_t13_cmdr_upd ON qp_t13_cmdr_t FOR UPDATE
             USING (qp_t13_cmdr_upd(id)) WITH CHECK (qp_t13_cmdr_upd(id));
         CREATE POLICY qp_t13_cmdr_del ON qp_t13_cmdr_t FOR DELETE
             USING (qp_t13_cmdr_del(id));",
    )]);
    for (role, function, denied_sql) in [
        (
            "qp_t13_cmdr_nosel",
            "qp_t13_cmdr_sel",
            &[
                "SELECT * FROM qp_t13_cmdr_t",
                "EXPLAIN SELECT * FROM qp_t13_cmdr_t",
                "INSERT INTO qp_t13_cmdr_t VALUES (1, 1) RETURNING id",
                "UPDATE qp_t13_cmdr_t SET x = 1 WHERE id = 1",
                "DELETE FROM qp_t13_cmdr_t WHERE id = 1",
            ][..],
        ),
        (
            "qp_t13_cmdr_noins",
            "qp_t13_cmdr_ins",
            &[
                "INSERT INTO qp_t13_cmdr_t VALUES (1, 1)",
                "EXPLAIN INSERT INTO qp_t13_cmdr_t VALUES (1, 1)",
            ][..],
        ),
        (
            "qp_t13_cmdr_noupd",
            "qp_t13_cmdr_upd",
            &[
                "UPDATE qp_t13_cmdr_t SET x = 1",
                "EXPLAIN UPDATE qp_t13_cmdr_t SET x = 1",
                "SELECT * FROM qp_t13_cmdr_t FOR UPDATE",
            ][..],
        ),
        (
            "qp_t13_cmdr_nodel",
            "qp_t13_cmdr_del",
            &[
                "DELETE FROM qp_t13_cmdr_t",
                "EXPLAIN DELETE FROM qp_t13_cmdr_t",
            ][..],
        ),
    ] {
        for sql in denied_sql {
            denied_with(
                &db,
                role,
                sql,
                &format!("permission denied for function {function}"),
            );
        }
    }
    // The statements that need no other command's policies stay allowed.
    for (role, sql) in [
        (
            "qp_t13_cmdr_nosel",
            "INSERT INTO qp_t13_cmdr_t VALUES (1, 1)",
        ),
        ("qp_t13_cmdr_nosel", "UPDATE qp_t13_cmdr_t SET x = 1"),
        ("qp_t13_cmdr_nosel", "DELETE FROM qp_t13_cmdr_t"),
        ("qp_t13_cmdr_noins", "SELECT * FROM qp_t13_cmdr_t"),
        ("qp_t13_cmdr_noupd", "SELECT * FROM qp_t13_cmdr_t"),
        ("qp_t13_cmdr_noupd", "DELETE FROM qp_t13_cmdr_t"),
        ("qp_t13_cmdr_nodel", "UPDATE qp_t13_cmdr_t SET x = 1"),
    ] {
        allowed(&db, role, sql);
    }
}

#[test]
fn row_security_applies_by_role_attribute() {
    // `check_enable_rls`: the owner bypasses row security unless `FORCE ROW
    // LEVEL SECURITY`; a BYPASSRLS role and a superuser always do. A
    // `FORCE` without `ENABLE` does not turn row security on. Verified on
    // PostgreSQL 18.7.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_attr_owner;
         CREATE ROLE qp_t13_attr_app;
         CREATE ROLE qp_t13_attr_bypass BYPASSRLS;
         CREATE ROLE qp_t13_attr_super SUPERUSER;
         CREATE SCHEMA qp_t13_attr;
         CREATE FUNCTION qp_t13_attr.guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_attr.guard(int) FROM PUBLIC;
         GRANT USAGE, CREATE ON SCHEMA qp_t13_attr TO qp_t13_attr_owner;
         GRANT USAGE ON SCHEMA qp_t13_attr
             TO qp_t13_attr_app, qp_t13_attr_bypass, qp_t13_attr_super;
         SET ROLE qp_t13_attr_owner;
         CREATE TABLE qp_t13_attr.t (id int);
         GRANT SELECT ON qp_t13_attr.t
             TO qp_t13_attr_app, qp_t13_attr_bypass;
         ALTER TABLE qp_t13_attr.t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_attr_p ON qp_t13_attr.t FOR SELECT
             USING (qp_t13_attr.guard(id));
         RESET ROLE;",
    )]);
    // The owner, a BYPASSRLS role and a superuser never run the policy.
    allowed(&db, "qp_t13_attr_owner", "SELECT * FROM qp_t13_attr.t");
    allowed(&db, "qp_t13_attr_bypass", "SELECT * FROM qp_t13_attr.t");
    allowed(&db, "qp_t13_attr_super", "SELECT * FROM qp_t13_attr.t");
    denied_with(
        &db,
        "qp_t13_attr_app",
        "SELECT * FROM qp_t13_attr.t",
        "permission denied for function guard",
    );
    // FORCE puts the owner under its own policy too.
    db.apply_sql("ALTER TABLE qp_t13_attr.t FORCE ROW LEVEL SECURITY;")
        .unwrap();
    denied_with(
        &db,
        "qp_t13_attr_owner",
        "SELECT * FROM qp_t13_attr.t",
        "permission denied for function guard",
    );
    db.apply_sql("ALTER TABLE qp_t13_attr.t NO FORCE ROW LEVEL SECURITY;")
        .unwrap();
    allowed(&db, "qp_t13_attr_owner", "SELECT * FROM qp_t13_attr.t");

    // FORCE without ENABLE leaves row security off.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_attr2_app;
         CREATE TABLE qp_t13_attr2_t (id int);
         GRANT SELECT ON qp_t13_attr2_t TO qp_t13_attr2_app;
         CREATE FUNCTION qp_t13_attr2_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_attr2_guard(int) FROM PUBLIC;
         ALTER TABLE qp_t13_attr2_t ENABLE ROW LEVEL SECURITY;
         ALTER TABLE qp_t13_attr2_t FORCE ROW LEVEL SECURITY;
         ALTER TABLE qp_t13_attr2_t DISABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_attr2_p ON qp_t13_attr2_t FOR SELECT
             USING (qp_t13_attr2_guard(id));",
    )]);
    allowed(&db, "qp_t13_attr2_app", "SELECT * FROM qp_t13_attr2_t");

    // A policy's TO role matches through memberships.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_attr3_group;
         CREATE ROLE qp_t13_attr3_member;
         CREATE ROLE qp_t13_attr3_other;
         GRANT qp_t13_attr3_group TO qp_t13_attr3_member;
         CREATE TABLE qp_t13_attr3_t (id int);
         GRANT SELECT ON qp_t13_attr3_t
             TO qp_t13_attr3_member, qp_t13_attr3_other;
         CREATE FUNCTION qp_t13_attr3_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_attr3_guard(int) FROM PUBLIC;
         ALTER TABLE qp_t13_attr3_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_attr3_p ON qp_t13_attr3_t FOR SELECT
             TO qp_t13_attr3_group USING (qp_t13_attr3_guard(id));",
    )]);
    denied_with(
        &db,
        "qp_t13_attr3_member",
        "SELECT * FROM qp_t13_attr3_t",
        "permission denied for function qp_t13_attr3_guard",
    );
    allowed(&db, "qp_t13_attr3_other", "SELECT * FROM qp_t13_attr3_t");

    // A role with the table owner's privileges bypasses row security like
    // the owner (object_ownercheck uses has_privs_of_role).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_attr4_owner;
         CREATE ROLE qp_t13_attr4_member;
         GRANT qp_t13_attr4_owner TO qp_t13_attr4_member;
         CREATE SCHEMA qp_t13_attr4;
         GRANT USAGE, CREATE ON SCHEMA qp_t13_attr4 TO qp_t13_attr4_owner;
         GRANT USAGE ON SCHEMA qp_t13_attr4 TO qp_t13_attr4_member;
         CREATE FUNCTION qp_t13_attr4_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_attr4_guard(int) FROM PUBLIC;
         SET ROLE qp_t13_attr4_owner;
         CREATE TABLE qp_t13_attr4.t (id int);
         GRANT SELECT ON qp_t13_attr4.t TO qp_t13_attr4_member;
         ALTER TABLE qp_t13_attr4.t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_attr4_p ON qp_t13_attr4.t FOR SELECT
             USING (qp_t13_attr4_guard(id));
         RESET ROLE;",
    )]);
    allowed(&db, "qp_t13_attr4_member", "SELECT * FROM qp_t13_attr4.t");
}

#[test]
fn row_security_policy_changes() {
    // ALTER POLICY replaces the clauses and the roles it applies to; DROP
    // POLICY leaves the default deny, which runs nothing. Verified on
    // PostgreSQL 18.7.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_alt_app;
         CREATE ROLE qp_t13_alt_other;
         CREATE TABLE qp_t13_alt_t (id int);
         GRANT SELECT ON qp_t13_alt_t TO qp_t13_alt_app, qp_t13_alt_other;
         CREATE FUNCTION qp_t13_alt_guard1(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_alt_guard2(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_alt_guard1(int),
             qp_t13_alt_guard2(int) FROM PUBLIC;
         ALTER TABLE qp_t13_alt_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_alt_p ON qp_t13_alt_t FOR SELECT
             TO qp_t13_alt_app USING (qp_t13_alt_guard1(id));",
    )]);
    denied_with(
        &db,
        "qp_t13_alt_app",
        "SELECT * FROM qp_t13_alt_t",
        "permission denied for function qp_t13_alt_guard1",
    );
    db.apply_sql("ALTER POLICY qp_t13_alt_p ON qp_t13_alt_t USING (qp_t13_alt_guard2(id));")
        .unwrap();
    denied_with(
        &db,
        "qp_t13_alt_app",
        "SELECT * FROM qp_t13_alt_t",
        "permission denied for function qp_t13_alt_guard2",
    );
    db.apply_sql("ALTER POLICY qp_t13_alt_p ON qp_t13_alt_t TO qp_t13_alt_other;")
        .unwrap();
    allowed(&db, "qp_t13_alt_app", "SELECT * FROM qp_t13_alt_t");
    denied_with(
        &db,
        "qp_t13_alt_other",
        "SELECT * FROM qp_t13_alt_t",
        "permission denied for function qp_t13_alt_guard2",
    );
    db.apply_sql("DROP POLICY qp_t13_alt_p ON qp_t13_alt_t;")
        .unwrap();
    allowed(&db, "qp_t13_alt_app", "SELECT * FROM qp_t13_alt_t");
    allowed(&db, "qp_t13_alt_other", "SELECT * FROM qp_t13_alt_t");
}

#[test]
fn row_security_policy_sublinks_and_stored_names() {
    // A policy's USING / WITH CHECK are stored analyzed: their function is
    // the OID it resolved to (no schema USAGE is checked at run time), and
    // a subquery in them puts its relations in the range table, whose
    // SELECT is checked as the statement's role. What the policy reads of
    // its own table's row needs no column privilege of the caller. Verified
    // on PostgreSQL 18.7.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_sub_app;
         CREATE SCHEMA qp_t13_sub_fn;
         CREATE FUNCTION qp_t13_sub_fn.guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_sub_fn.guard(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t13_sub_fn.guard(int) TO qp_t13_sub_app;
         CREATE TABLE qp_t13_sub_t (id int, tenant int);
         GRANT SELECT ON qp_t13_sub_t TO qp_t13_sub_app;
         ALTER TABLE qp_t13_sub_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_sub_p ON qp_t13_sub_t FOR SELECT
             USING (qp_t13_sub_fn.guard(tenant));",
    )]);
    // The role has no USAGE on qp_t13_sub_fn: the stored expression needs
    // none.
    allowed(&db, "qp_t13_sub_app", "SELECT * FROM qp_t13_sub_t");
    db.apply_sql("REVOKE EXECUTE ON FUNCTION qp_t13_sub_fn.guard(int) FROM qp_t13_sub_app;")
        .unwrap();
    denied_with(
        &db,
        "qp_t13_sub_app",
        "SELECT * FROM qp_t13_sub_t",
        "permission denied for function guard",
    );

    // A policy's EXISTS subquery reads a relation the caller must see.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_sub2_app;
         CREATE TABLE qp_t13_sub2_t (id int, tenant int);
         CREATE TABLE qp_t13_sub2_members (uid int, tenant int);
         GRANT SELECT ON qp_t13_sub2_t TO qp_t13_sub2_app;
         ALTER TABLE qp_t13_sub2_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_sub2_p ON qp_t13_sub2_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t13_sub2_members m
                            WHERE m.tenant = qp_t13_sub2_t.tenant));",
    )]);
    denied_with(
        &db,
        "qp_t13_sub2_app",
        "SELECT * FROM qp_t13_sub2_t",
        "permission denied for table qp_t13_sub2_members",
    );
    denied_with(
        &db,
        "qp_t13_sub2_app",
        "EXPLAIN SELECT * FROM qp_t13_sub2_t",
        "permission denied for table qp_t13_sub2_members",
    );

    // Only the column the subquery reads is required.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_sub3_app;
         CREATE TABLE qp_t13_sub3_t (id int, tenant int);
         CREATE TABLE qp_t13_sub3_members (uid int, tenant int);
         GRANT SELECT ON qp_t13_sub3_t TO qp_t13_sub3_app;
         GRANT SELECT (tenant) ON qp_t13_sub3_members TO qp_t13_sub3_app;
         ALTER TABLE qp_t13_sub3_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_sub3_p ON qp_t13_sub3_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t13_sub3_members m
                            WHERE m.tenant = qp_t13_sub3_t.tenant));",
    )]);
    allowed(&db, "qp_t13_sub3_app", "SELECT * FROM qp_t13_sub3_t");

    // The policy's own row columns are not the statement's.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_sub4_app;
         CREATE TABLE qp_t13_sub4_t (id int, tenant int);
         GRANT SELECT (id) ON qp_t13_sub4_t TO qp_t13_sub4_app;
         ALTER TABLE qp_t13_sub4_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_sub4_p ON qp_t13_sub4_t FOR SELECT
             USING (tenant = 1);",
    )]);
    allowed(&db, "qp_t13_sub4_app", "SELECT id FROM qp_t13_sub4_t");
    denied_with(
        &db,
        "qp_t13_sub4_app",
        "SELECT tenant FROM qp_t13_sub4_t",
        "permission denied for table qp_t13_sub4_t",
    );

    // A typical `current_setting` policy runs no user code.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_sub5_app;
         CREATE TABLE qp_t13_sub5_t (id int, tenant int);
         GRANT SELECT ON qp_t13_sub5_t TO qp_t13_sub5_app;
         ALTER TABLE qp_t13_sub5_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_sub5_p ON qp_t13_sub5_t FOR SELECT
             USING (tenant = current_setting('server_version_num')::int);",
    )]);
    allowed(&db, "qp_t13_sub5_app", "SELECT * FROM qp_t13_sub5_t");

    // The body of a policy's function is checked as the caller when the
    // function runs (not by an EXPLAIN, which initializes but never runs
    // it).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_sub6_app;
         CREATE TABLE qp_t13_sub6_secret (x int);
         CREATE FUNCTION qp_t13_sub6_guard(int) RETURNS boolean
             LANGUAGE sql IMMUTABLE
             AS $$ SELECT (SELECT max(x)::int FROM qp_t13_sub6_secret) > 0 $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_sub6_guard(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t13_sub6_guard(int) TO qp_t13_sub6_app;
         CREATE TABLE qp_t13_sub6_t (id int);
         GRANT SELECT ON qp_t13_sub6_t TO qp_t13_sub6_app;
         ALTER TABLE qp_t13_sub6_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_sub6_p ON qp_t13_sub6_t FOR SELECT
             USING (qp_t13_sub6_guard(id));",
    )]);
    denied_with(
        &db,
        "qp_t13_sub6_app",
        "SELECT * FROM qp_t13_sub6_t",
        "permission denied for table qp_t13_sub6_secret",
    );
    allowed(
        &db,
        "qp_t13_sub6_app",
        "EXPLAIN SELECT * FROM qp_t13_sub6_t",
    );

    // A cast runs its function: the policy stores the `FuncExpr` the cast
    // became, and the executor initializes it like any other.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_sub7_app;
         CREATE TYPE qp_t13_sub7_ta AS (v int);
         CREATE TYPE qp_t13_sub7_tb AS (v int);
         CREATE FUNCTION qp_t13_sub7_to_tb(qp_t13_sub7_ta)
             RETURNS qp_t13_sub7_tb LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN ROW($1.v)::qp_t13_sub7_tb; END $$;
         CREATE CAST (qp_t13_sub7_ta AS qp_t13_sub7_tb)
             WITH FUNCTION qp_t13_sub7_to_tb(qp_t13_sub7_ta);
         CREATE TABLE qp_t13_sub7_t (x qp_t13_sub7_ta);
         GRANT SELECT ON qp_t13_sub7_t TO qp_t13_sub7_app;
         REVOKE EXECUTE ON FUNCTION qp_t13_sub7_to_tb(qp_t13_sub7_ta)
             FROM PUBLIC;
         ALTER TABLE qp_t13_sub7_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_sub7_p ON qp_t13_sub7_t FOR SELECT
             USING ((x::qp_t13_sub7_tb) IS NOT NULL);",
    )]);
    for sql in [
        "SELECT * FROM qp_t13_sub7_t",
        "EXPLAIN SELECT * FROM qp_t13_sub7_t",
    ] {
        denied_with(
            &db,
            "qp_t13_sub7_app",
            sql,
            "permission denied for function qp_t13_sub7_to_tb",
        );
    }
}

#[test]
fn row_security_policies_through_views() {
    // A table reached through a view applies its row security as the view's
    // owner (`checkAsUser`: whose policies apply, and whose SELECT the
    // policy's subqueries need), while the policy expressions' functions
    // are initialized as the invoking role (`ExecInitFunc`). A
    // `security_invoker` view applies the invoker's. Verified on
    // PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_vw_admin;
         CREATE ROLE qp_t13_vw_owner;
         CREATE ROLE qp_t13_vw_app;
         CREATE SCHEMA qp_t13_vw;
         GRANT USAGE, CREATE ON SCHEMA qp_t13_vw
             TO qp_t13_vw_admin, qp_t13_vw_owner;
         GRANT USAGE ON SCHEMA qp_t13_vw TO qp_t13_vw_app;
         SET ROLE qp_t13_vw_admin;
         CREATE FUNCTION qp_t13_vw.guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE TABLE qp_t13_vw.t (id int);
         CREATE TABLE qp_t13_vw.secret (id int);
         ALTER TABLE qp_t13_vw.t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_vw_p ON qp_t13_vw.t FOR SELECT
             USING (qp_t13_vw.guard(id)
                    AND EXISTS (SELECT 1 FROM qp_t13_vw.secret s
                                WHERE s.id = qp_t13_vw.t.id));
         GRANT SELECT ON qp_t13_vw.t, qp_t13_vw.secret TO qp_t13_vw_owner;
         GRANT SELECT ON qp_t13_vw.t TO qp_t13_vw_app;
         GRANT EXECUTE ON FUNCTION qp_t13_vw.guard(int) TO qp_t13_vw_owner;
         REVOKE EXECUTE ON FUNCTION qp_t13_vw.guard(int) FROM PUBLIC;
         RESET ROLE;
         SET ROLE qp_t13_vw_owner;
         CREATE VIEW qp_t13_vw.v AS SELECT id FROM qp_t13_vw.t;
         GRANT SELECT ON qp_t13_vw.v TO qp_t13_vw_app;
         RESET ROLE;",
    )]);
    // The policy applies as the view's owner (it has SELECT on secret), but
    // its function's EXECUTE is the caller's: no EXECUTE, no query.
    denied_with(
        &db,
        "qp_t13_vw_app",
        "SELECT * FROM qp_t13_vw.v",
        "permission denied for function guard",
    );
    // The invoking role reads the table directly: its own policies apply.
    denied_with(
        &db,
        "qp_t13_vw_app",
        "SELECT * FROM qp_t13_vw.t",
        "permission denied for table secret",
    );

    // A security_invoker view reads the table as the caller: the policy's
    // function is the caller's, and so is its subquery's SELECT.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_vw2_admin;
         CREATE ROLE qp_t13_vw2_app;
         CREATE SCHEMA qp_t13_vw2;
         GRANT USAGE, CREATE ON SCHEMA qp_t13_vw2 TO qp_t13_vw2_admin;
         GRANT USAGE ON SCHEMA qp_t13_vw2 TO qp_t13_vw2_app;
         SET ROLE qp_t13_vw2_admin;
         CREATE FUNCTION qp_t13_vw2.guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE TABLE qp_t13_vw2.t (id int);
         ALTER TABLE qp_t13_vw2.t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_vw2_p ON qp_t13_vw2.t FOR SELECT
             USING (qp_t13_vw2.guard(id));
         GRANT SELECT ON qp_t13_vw2.t TO qp_t13_vw2_app;
         REVOKE EXECUTE ON FUNCTION qp_t13_vw2.guard(int) FROM PUBLIC;
         CREATE VIEW qp_t13_vw2.vi WITH (security_invoker = true)
             AS SELECT id FROM qp_t13_vw2.t;
         GRANT SELECT ON qp_t13_vw2.vi TO qp_t13_vw2_app;
         RESET ROLE;",
    )]);
    denied_with(
        &db,
        "qp_t13_vw2_app",
        "SELECT * FROM qp_t13_vw2.vi",
        "permission denied for function guard",
    );

    // A write through an auto-updatable view takes the base table's INSERT
    // policies, as the view's owner; their WITH CHECK functions are the
    // caller's.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_vw3_admin;
         CREATE ROLE qp_t13_vw3_owner;
         CREATE ROLE qp_t13_vw3_app;
         CREATE SCHEMA qp_t13_vw3;
         GRANT USAGE, CREATE ON SCHEMA qp_t13_vw3
             TO qp_t13_vw3_admin, qp_t13_vw3_owner;
         GRANT USAGE ON SCHEMA qp_t13_vw3 TO qp_t13_vw3_app;
         SET ROLE qp_t13_vw3_admin;
         CREATE FUNCTION qp_t13_vw3.check_ins(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE TABLE qp_t13_vw3.t (id int);
         GRANT SELECT, INSERT ON qp_t13_vw3.t TO qp_t13_vw3_owner;
         ALTER TABLE qp_t13_vw3.t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_vw3_ins ON qp_t13_vw3.t FOR INSERT
             WITH CHECK (qp_t13_vw3.check_ins(id));
         REVOKE EXECUTE ON FUNCTION qp_t13_vw3.check_ins(int) FROM PUBLIC;
         RESET ROLE;
         SET ROLE qp_t13_vw3_owner;
         CREATE VIEW qp_t13_vw3.v AS SELECT id FROM qp_t13_vw3.t;
         GRANT INSERT ON qp_t13_vw3.v TO qp_t13_vw3_app;
         RESET ROLE;",
    )]);
    denied_with(
        &db,
        "qp_t13_vw3_app",
        "INSERT INTO qp_t13_vw3.v VALUES (1)",
        "permission denied for function check_ins",
    );
}

#[test]
fn row_security_operator_functions() {
    // A policy expression's operators are initializer steps of the plan:
    // their function's EXECUTE is checked like any other's. Verified on
    // PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_op_app;
         CREATE FUNCTION qp_t13_op_eq(int, int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_op_eq(int, int) FROM PUBLIC;
         CREATE OPERATOR ?# (LEFTARG = int, RIGHTARG = int,
             FUNCTION = qp_t13_op_eq);
         CREATE TABLE qp_t13_op_t (id int);
         GRANT SELECT ON qp_t13_op_t TO qp_t13_op_app;
         ALTER TABLE qp_t13_op_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_op_p ON qp_t13_op_t FOR SELECT
             USING (id ?# 1);",
    )]);
    denied_with(
        &db,
        "qp_t13_op_app",
        "SELECT * FROM qp_t13_op_t",
        "permission denied for function qp_t13_op_eq",
    );
}

#[test]
fn row_security_merge_policies() {
    // A MERGE target checks the UPDATE policies (the existing row through
    // the forced USING check, the new one through WITH CHECK), the DELETE
    // policies on the existing row and the INSERT policies on the new row,
    // whether or not an action can reach the row; the SELECT policies too
    // when the statement reads the target. Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_mrg_nosel;
         CREATE ROLE qp_t13_mrg_noins;
         CREATE ROLE qp_t13_mrg_noupd;
         CREATE ROLE qp_t13_mrg_nodel;
         CREATE TABLE qp_t13_mrg_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_t13_mrg_t
             TO qp_t13_mrg_nosel, qp_t13_mrg_noins,
                qp_t13_mrg_noupd, qp_t13_mrg_nodel;
         ALTER TABLE qp_t13_mrg_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_mrg_sel(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_mrg_ins(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_mrg_upd(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_mrg_del(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_mrg_sel(int), qp_t13_mrg_ins(int),
             qp_t13_mrg_upd(int), qp_t13_mrg_del(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t13_mrg_ins(int), qp_t13_mrg_upd(int),
             qp_t13_mrg_del(int) TO qp_t13_mrg_nosel;
         GRANT EXECUTE ON FUNCTION qp_t13_mrg_sel(int), qp_t13_mrg_upd(int),
             qp_t13_mrg_del(int) TO qp_t13_mrg_noins;
         GRANT EXECUTE ON FUNCTION qp_t13_mrg_sel(int), qp_t13_mrg_ins(int),
             qp_t13_mrg_del(int) TO qp_t13_mrg_noupd;
         GRANT EXECUTE ON FUNCTION qp_t13_mrg_sel(int), qp_t13_mrg_ins(int),
             qp_t13_mrg_upd(int) TO qp_t13_mrg_nodel;
         CREATE POLICY qp_t13_mrg_sel ON qp_t13_mrg_t FOR SELECT
             USING (qp_t13_mrg_sel(id));
         CREATE POLICY qp_t13_mrg_ins ON qp_t13_mrg_t FOR INSERT
             WITH CHECK (qp_t13_mrg_ins(id));
         CREATE POLICY qp_t13_mrg_upd ON qp_t13_mrg_t FOR UPDATE
             USING (qp_t13_mrg_upd(id)) WITH CHECK (qp_t13_mrg_upd(id));
         CREATE POLICY qp_t13_mrg_del ON qp_t13_mrg_t FOR DELETE
             USING (qp_t13_mrg_del(id));",
    )]);
    for (role, function) in [
        ("qp_t13_mrg_nosel", "qp_t13_mrg_sel"),
        ("qp_t13_mrg_noins", "qp_t13_mrg_ins"),
        ("qp_t13_mrg_noupd", "qp_t13_mrg_upd"),
        ("qp_t13_mrg_nodel", "qp_t13_mrg_del"),
    ] {
        denied_with(
            &db,
            role,
            "MERGE INTO qp_t13_mrg_t t USING (VALUES (1)) s(id) ON t.id = s.id
                 WHEN MATCHED THEN UPDATE SET x = 1
                 WHEN NOT MATCHED THEN INSERT (id, x) VALUES (1, 1)",
            &format!("permission denied for function {function}"),
        );
    }
    // A DO NOTHING action needs SELECT privileges, so the SELECT policies
    // are applied.
    denied_with(
        &db,
        "qp_t13_mrg_nosel",
        "MERGE INTO qp_t13_mrg_t t USING (VALUES (1)) s(id) ON t.id = s.id
             WHEN MATCHED THEN DO NOTHING",
        "permission denied for function qp_t13_mrg_sel",
    );

    // A MERGE that reads no target column needs no SELECT, and its SELECT
    // policies stay unbuilt.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_mrg2_app;
         CREATE TABLE qp_t13_mrg2_t (id int);
         GRANT INSERT, UPDATE, DELETE ON qp_t13_mrg2_t TO qp_t13_mrg2_app;
         ALTER TABLE qp_t13_mrg2_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t13_mrg2_sel(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_mrg2_ins(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_mrg2_upd(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_mrg2_del(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_mrg2_sel(int), qp_t13_mrg2_ins(int),
             qp_t13_mrg2_upd(int), qp_t13_mrg2_del(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t13_mrg2_ins(int), qp_t13_mrg2_upd(int),
             qp_t13_mrg2_del(int) TO qp_t13_mrg2_app;
         CREATE POLICY qp_t13_mrg2_sel ON qp_t13_mrg2_t FOR SELECT
             USING (qp_t13_mrg2_sel(id));
         CREATE POLICY qp_t13_mrg2_ins ON qp_t13_mrg2_t FOR INSERT
             WITH CHECK (qp_t13_mrg2_ins(id));
         CREATE POLICY qp_t13_mrg2_upd ON qp_t13_mrg2_t FOR UPDATE
             USING (qp_t13_mrg2_upd(id)) WITH CHECK (qp_t13_mrg2_upd(id));
         CREATE POLICY qp_t13_mrg2_del ON qp_t13_mrg2_t FOR DELETE
             USING (qp_t13_mrg2_del(id));",
    )]);
    allowed(
        &db,
        "qp_t13_mrg2_app",
        "MERGE INTO qp_t13_mrg2_t t USING (VALUES (1)) s(id) ON true
             WHEN NOT MATCHED THEN INSERT (id) VALUES (1)",
    );
}

#[test]
fn row_security_partitioned_tables() {
    // A partitioned table's policies apply when the relation is read
    // through it; a partition reached directly applies its own, never its
    // parent's (`RELPKIND_PARTITIONED_TABLE` vs a plain table RTE).
    // Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_part_app;
         CREATE TABLE qp_t13_part_p (id int) PARTITION BY RANGE (id);
         CREATE TABLE qp_t13_part_p1 PARTITION OF qp_t13_part_p
             FOR VALUES FROM (0) TO (10);
         GRANT SELECT ON qp_t13_part_p, qp_t13_part_p1 TO qp_t13_part_app;
         CREATE FUNCTION qp_t13_part_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_part_guard(int) FROM PUBLIC;
         ALTER TABLE qp_t13_part_p ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_part_pol ON qp_t13_part_p FOR SELECT
             USING (qp_t13_part_guard(id));",
    )]);
    denied_with(
        &db,
        "qp_t13_part_app",
        "SELECT * FROM qp_t13_part_p",
        "permission denied for function qp_t13_part_guard",
    );
    allowed(&db, "qp_t13_part_app", "SELECT * FROM qp_t13_part_p1");

    // The partition's own policy applies to a direct read.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_part2_app;
         CREATE TABLE qp_t13_part2_p (id int) PARTITION BY RANGE (id);
         CREATE TABLE qp_t13_part2_p1 PARTITION OF qp_t13_part2_p
             FOR VALUES FROM (0) TO (10);
         GRANT SELECT ON qp_t13_part2_p, qp_t13_part2_p1 TO qp_t13_part2_app;
         CREATE FUNCTION qp_t13_part2_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t13_part2_guard2(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_part2_guard(int),
             qp_t13_part2_guard2(int) FROM PUBLIC;
         ALTER TABLE qp_t13_part2_p ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_part2_pol ON qp_t13_part2_p FOR SELECT
             USING (qp_t13_part2_guard(id));
         ALTER TABLE qp_t13_part2_p1 ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t13_part2_pol1 ON qp_t13_part2_p1 FOR SELECT
             USING (qp_t13_part2_guard2(id));",
    )]);
    denied_with(
        &db,
        "qp_t13_part2_app",
        "SELECT * FROM qp_t13_part2_p1",
        "permission denied for function qp_t13_part2_guard2",
    );
    denied_with(
        &db,
        "qp_t13_part2_app",
        "SELECT * FROM qp_t13_part2_p",
        "permission denied for function qp_t13_part2_guard",
    );
}

#[test]
fn single_item_in_over_row_constructors_is_fieldwise() {
    // `transformAExprIn` compares the *transformed* operands: a `ROW(...)`,
    // bare or cast to a composite type, is a row constructor there (the cast
    // rebuilds the row with its fields coerced), so each field pair is
    // compared with its own type's operator and the composite type's `=`
    // never runs. `transformAExprOp` instead checks the raw operands, so
    // `ROW(...)::ct = ROW(...)::ct` is the composite's `=`. A composite-typed
    // column is no row constructor. Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_in1_app;
         CREATE TYPE qp_t13_in1_ct AS (a int, b int);
         CREATE FUNCTION qp_t13_in1_eq(qp_t13_in1_ct, qp_t13_in1_ct)
             RETURNS boolean LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_in1_eq(qp_t13_in1_ct, qp_t13_in1_ct)
             FROM PUBLIC;
         CREATE OPERATOR = (LEFTARG = qp_t13_in1_ct,
             RIGHTARG = qp_t13_in1_ct, FUNCTION = qp_t13_in1_eq);
         CREATE TABLE qp_t13_in1_t (id int, a int, b int, d qp_t13_in1_ct);
         GRANT SELECT ON qp_t13_in1_t TO qp_t13_in1_app;",
    )]);
    // Field-wise comparisons: the composite `=` is never called.
    for sql in [
        "SELECT ROW(a,b) IN (ROW(1,2)) FROM qp_t13_in1_t",
        "SELECT (a,b) IN ((1,2)) FROM qp_t13_in1_t",
        "SELECT ROW(a,b) IN (ROW(1,2), ROW(3,4)) FROM qp_t13_in1_t",
        "SELECT (a,b) IN ((1,2),(3,4)) FROM qp_t13_in1_t",
        "SELECT (a,b)::qp_t13_in1_ct IN ((1,2)::qp_t13_in1_ct)
             FROM qp_t13_in1_t",
        "SELECT ROW(a,b)::qp_t13_in1_ct IN (ROW(1,2)::qp_t13_in1_ct)
             FROM qp_t13_in1_t",
        "SELECT ROW(a,b)::qp_t13_in1_ct IN (ROW(1,2)) FROM qp_t13_in1_t",
        "SELECT (a,b)::qp_t13_in1_ct IN ((1,2)) FROM qp_t13_in1_t",
        "SELECT ROW(a,b)::record IN (ROW(1,2)) FROM qp_t13_in1_t",
        "SELECT ROW(a,b) IN (ROW(1,2)::record) FROM qp_t13_in1_t",
        "SELECT ROW(a,b)::qp_t13_in1_ct IN (ROW(1,2), ROW(3,4))
             FROM qp_t13_in1_t",
        "SELECT ROW(a,b) IN (ROW(1,2)::qp_t13_in1_ct, ROW(3,4)::qp_t13_in1_ct)
             FROM qp_t13_in1_t",
        "SELECT ROW(1,2) IN (ROW(1,2))",
        "SELECT ROW(1,2)::qp_t13_in1_ct IN (ROW(1,2)::qp_t13_in1_ct)",
    ] {
        allowed(&db, "qp_t13_in1_app", sql);
    }
    // Scalar comparisons of the composite: its `=` is called.
    for sql in [
        "SELECT d IN (d) FROM qp_t13_in1_t",
        "SELECT d = d FROM qp_t13_in1_t",
        "SELECT d IN (d, d) FROM qp_t13_in1_t",
        "SELECT d IN (ROW(1,2)::qp_t13_in1_ct) FROM qp_t13_in1_t",
        "SELECT ROW(a,b)::qp_t13_in1_ct IN (d) FROM qp_t13_in1_t",
        "SELECT ROW(a,b)::qp_t13_in1_ct = ROW(1,2)::qp_t13_in1_ct
             FROM qp_t13_in1_t",
        "SELECT d IN (ROW(1,2)::qp_t13_in1_ct, ROW(3,4)::qp_t13_in1_ct)
             FROM qp_t13_in1_t",
        "SELECT ROW(a,b)::qp_t13_in1_ct IN
             (ROW(1,2)::qp_t13_in1_ct, ROW(3,4)::qp_t13_in1_ct)
             FROM qp_t13_in1_t",
        "EXPLAIN SELECT d IN (ROW(1,2)::qp_t13_in1_ct) FROM qp_t13_in1_t",
    ] {
        denied_with(
            &db,
            "qp_t13_in1_app",
            sql,
            "permission denied for function qp_t13_in1_eq",
        );
    }
    // A row constructor against a composite-typed value: no operator.
    for (sql, prefix) in [
        (
            "SELECT d IN (ROW(1,2)) FROM qp_t13_in1_t",
            "operator is not unique: qp_t13_in1_ct = record",
        ),
        (
            "SELECT ROW(a,b) IN (d) FROM qp_t13_in1_t",
            "operator is not unique: record = qp_t13_in1_ct",
        ),
        (
            "SELECT '(1,2)'::qp_t13_in1_ct IN (ROW(1,2)) FROM qp_t13_in1_t",
            "operator is not unique: qp_t13_in1_ct = record",
        ),
        (
            "SELECT ROW(1,2) IN ('(1,2)'::qp_t13_in1_ct) FROM qp_t13_in1_t",
            "operator is not unique: record = qp_t13_in1_ct",
        ),
    ] {
        denied_with(&db, "qp_t13_in1_app", sql, prefix);
    }

    // A field of a composite type keeps its own `=` in the field-wise
    // comparison; the outer composite's `=` is only for the scalar forms.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t13_in2_app;
         CREATE TYPE qp_t13_in2_one AS (v int);
         CREATE FUNCTION qp_t13_in2_one_eq(qp_t13_in2_one, qp_t13_in2_one)
             RETURNS boolean LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_in2_one_eq(
             qp_t13_in2_one, qp_t13_in2_one) FROM PUBLIC;
         CREATE OPERATOR = (LEFTARG = qp_t13_in2_one,
             RIGHTARG = qp_t13_in2_one, FUNCTION = qp_t13_in2_one_eq);
         CREATE TYPE qp_t13_in2_wrap AS (p qp_t13_in2_one);
         CREATE FUNCTION qp_t13_in2_wrap_eq(qp_t13_in2_wrap, qp_t13_in2_wrap)
             RETURNS boolean LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t13_in2_wrap_eq(
             qp_t13_in2_wrap, qp_t13_in2_wrap) FROM PUBLIC;
         CREATE OPERATOR = (LEFTARG = qp_t13_in2_wrap,
             RIGHTARG = qp_t13_in2_wrap, FUNCTION = qp_t13_in2_wrap_eq);
         CREATE TABLE qp_t13_in2_t (w qp_t13_in2_wrap);
         GRANT SELECT ON qp_t13_in2_t TO qp_t13_in2_app;",
    )]);
    denied_with(
        &db,
        "qp_t13_in2_app",
        "SELECT ROW(ROW(1)::qp_t13_in2_one)::qp_t13_in2_wrap IN
             (ROW(ROW(1)::qp_t13_in2_one)::qp_t13_in2_wrap)
             FROM qp_t13_in2_t",
        "permission denied for function qp_t13_in2_one_eq",
    );
    denied_with(
        &db,
        "qp_t13_in2_app",
        "SELECT ROW(ROW(1)::qp_t13_in2_one)::qp_t13_in2_wrap =
             ROW(ROW(1)::qp_t13_in2_one)::qp_t13_in2_wrap
             FROM qp_t13_in2_t",
        "permission denied for function qp_t13_in2_wrap_eq",
    );
    db.apply_sql(
        "GRANT EXECUTE ON FUNCTION qp_t13_in2_one_eq(
             qp_t13_in2_one, qp_t13_in2_one) TO qp_t13_in2_app;",
    )
    .unwrap();
    allowed(
        &db,
        "qp_t13_in2_app",
        "SELECT ROW(ROW(1)::qp_t13_in2_one)::qp_t13_in2_wrap IN
             (ROW(ROW(1)::qp_t13_in2_one)::qp_t13_in2_wrap)
             FROM qp_t13_in2_t",
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Ninth round (T14): `row_security` off, policy recursion, stored policies
// through renames, and INSERT … SELECT / ON CONFLICT policy shapes
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn row_security_off_refuses_instead_of_filtering() {
    // `check_enable_rls`: a session whose `row_security` is off gets
    // `query would be affected by row-level security policy for table "t"`
    // (SQLSTATE 42501) wherever row security would apply — whatever the
    // policies would filter, and before any of them is looked at. The owner
    // bypasses unless `FORCE ROW LEVEL SECURITY` made row security apply to
    // it, and then PG adds its hint; BYPASSRLS and superusers never see the
    // error. A role's session setting comes from `ALTER ROLE` (also
    // `IN DATABASE`), the database's or the every-role one. Verified on
    // PostgreSQL 18.7.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_off_app;
         CREATE ROLE qp_t14_off_owner;
         CREATE ROLE qp_t14_off_bypass BYPASSRLS;
         CREATE ROLE qp_t14_off_super SUPERUSER;
         GRANT USAGE, CREATE ON SCHEMA public TO qp_t14_off_owner;
         GRANT USAGE ON SCHEMA public
             TO qp_t14_off_app, qp_t14_off_bypass, qp_t14_off_super;
         SET ROLE qp_t14_off_owner;
         CREATE TABLE qp_t14_off_t (id int);
         RESET ROLE;
         GRANT SELECT ON qp_t14_off_t
             TO qp_t14_off_app, qp_t14_off_bypass, qp_t14_off_super;
         ALTER TABLE qp_t14_off_t ENABLE ROW LEVEL SECURITY;
         ALTER ROLE qp_t14_off_app SET row_security = off;",
    )]);
    let message = "query would be affected by row-level security policy for table \"qp_t14_off_t\"";
    // With or without a policy the default-deny/qualifying ones don't
    // matter: the error is raised while rewriting the statement.
    assert_eq!(
        denied(&db, "qp_t14_off_app", "SELECT * FROM qp_t14_off_t"),
        message
    );
    db.apply_sql("CREATE POLICY qp_t14_off_p ON qp_t14_off_t FOR SELECT USING (true);")
        .unwrap();
    assert_eq!(
        denied(&db, "qp_t14_off_app", "SELECT * FROM qp_t14_off_t"),
        message
    );
    // EXPLAIN runs the rewriter too.
    assert_eq!(
        denied(&db, "qp_t14_off_app", "EXPLAIN SELECT * FROM qp_t14_off_t"),
        message
    );
    // BYPASSRLS and superuser roles bypass row security altogether.
    allowed(&db, "qp_t14_off_bypass", "SELECT * FROM qp_t14_off_t");
    allowed(&db, "qp_t14_off_super", "SELECT * FROM qp_t14_off_t");
    // The owner bypasses, unless FORCE makes row security apply to it; then
    // the error carries PG's hint.
    db.apply_sql("ALTER ROLE qp_t14_off_owner SET row_security = off;")
        .unwrap();
    allowed(&db, "qp_t14_off_owner", "SELECT * FROM qp_t14_off_t");
    db.apply_sql("ALTER TABLE qp_t14_off_t FORCE ROW LEVEL SECURITY;")
        .unwrap();
    let e = denied(&db, "qp_t14_off_owner", "SELECT * FROM qp_t14_off_t");
    assert!(e.starts_with(message), "{e}");
    assert!(
        e.contains(
            "To disable the policy for the table's owner, use ALTER TABLE NO FORCE ROW LEVEL \
             SECURITY."
        ),
        "{e}"
    );
    // Row security off on the table: nothing applies.
    db.apply_sql("ALTER TABLE qp_t14_off_t DISABLE ROW LEVEL SECURITY;")
        .unwrap();
    allowed(&db, "qp_t14_off_app", "SELECT * FROM qp_t14_off_t");
    db.apply_sql("ALTER TABLE qp_t14_off_t ENABLE ROW LEVEL SECURITY;")
        .unwrap();
    // RESET drops the setting. (`ALTER ROLE ALL` is cluster-wide: it would
    // reach every other test's roles on the oracle's shared server, so it is
    // covered by `row_security_off_in_database_is_a_role_setting`, off the
    // oracle.)
    db.apply_sql("ALTER ROLE qp_t14_off_app RESET row_security;")
        .unwrap();
    allowed(&db, "qp_t14_off_app", "SELECT * FROM qp_t14_off_t");
    // A view's query is checked as its owner (`checkAsUser`), but the GUC
    // is the session's: the owner's own setting doesn't reach the caller's
    // session, so the read is filtered; the caller's off rejects it (with
    // the owner's hint, FORCE having made row security apply to the owner).
    db.apply_sql(
        "SET ROLE qp_t14_off_owner;
         CREATE VIEW qp_t14_off_v AS SELECT * FROM public.qp_t14_off_t;
         RESET ROLE;
         GRANT SELECT ON qp_t14_off_v TO qp_t14_off_app;",
    )
    .unwrap();
    allowed(&db, "qp_t14_off_app", "SELECT * FROM qp_t14_off_v");
    db.apply_sql("ALTER ROLE qp_t14_off_app SET row_security = off;")
        .unwrap();
    let e = denied(&db, "qp_t14_off_app", "SELECT * FROM qp_t14_off_v");
    assert!(e.starts_with(message), "{e}");
    assert!(
        e.contains("To disable the policy for the table's owner"),
        "{e}"
    );
}

#[test]
fn row_security_off_in_database_is_a_role_setting() {
    // `ALTER ROLE r IN DATABASE d SET row_security = off`: the analyzer
    // can't check the database name (it models the application's one), so
    // the setting applies. `ALTER DATABASE ... SET` does too (skip the
    // oracle: neither database name exists on its server).
    let mut db = PgCatalog::new().unwrap();
    db.skip_pg_sanity();
    db.apply_sql(
        "CREATE ROLE qp_t14_offdb_app;
         CREATE TABLE qp_t14_offdb_t (id int);
         GRANT SELECT ON qp_t14_offdb_t TO qp_t14_offdb_app;
         ALTER TABLE qp_t14_offdb_t ENABLE ROW LEVEL SECURITY;
         ALTER ROLE qp_t14_offdb_app IN DATABASE appdb SET row_security = off;",
    )
    .unwrap();
    assert_eq!(
        denied(&db, "qp_t14_offdb_app", "SELECT * FROM qp_t14_offdb_t"),
        "query would be affected by row-level security policy for table \"qp_t14_offdb_t\""
    );
    db.apply_sql("ALTER ROLE qp_t14_offdb_app IN DATABASE appdb RESET row_security;")
        .unwrap();
    allowed(&db, "qp_t14_offdb_app", "SELECT * FROM qp_t14_offdb_t");
    db.apply_sql("ALTER DATABASE appdb SET row_security = off;")
        .unwrap();
    denied(&db, "qp_t14_offdb_app", "SELECT * FROM qp_t14_offdb_t");
    db.apply_sql("ALTER DATABASE appdb RESET row_security;")
        .unwrap();
    allowed(&db, "qp_t14_offdb_app", "SELECT * FROM qp_t14_offdb_t");
    // `ALTER ROLE ALL` sets every role's (cluster-wide, so not for the
    // oracle's shared server).
    db.apply_sql("ALTER ROLE ALL SET row_security = off;")
        .unwrap();
    denied(&db, "qp_t14_offdb_app", "SELECT * FROM qp_t14_offdb_t");
    db.apply_sql("ALTER ROLE ALL RESET ALL;").unwrap();
    allowed(&db, "qp_t14_offdb_app", "SELECT * FROM qp_t14_offdb_t");
}

#[test]
fn row_security_policy_recursion_is_rejected() {
    // `fireRIRrules`: when a policy's expression holds a subquery, the
    // rewriter expands the new security quals with the relation marked
    // active; reaching it again raises `infinite recursion detected in
    // policy for relation "t"` (SQLSTATE 42P17) on every execution. A
    // policy without a subquery can't reach another relation; a function
    // call is not expanded at rewrite time (its body is a statement of its
    // own, run later), so it never raises this error. Verified on
    // PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_rec_app;
         CREATE TABLE qp_t14_rec_t (id int);
         GRANT SELECT ON qp_t14_rec_t TO qp_t14_rec_app;
         ALTER TABLE qp_t14_rec_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t14_rec_p ON qp_t14_rec_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t14_rec_t x
                            WHERE x.id = qp_t14_rec_t.id));",
    )]);
    let message = "infinite recursion detected in policy for relation \"qp_t14_rec_t\"";
    assert_eq!(
        denied(&db, "qp_t14_rec_app", "SELECT * FROM qp_t14_rec_t"),
        message
    );
    assert_eq!(
        denied(&db, "qp_t14_rec_app", "EXPLAIN SELECT * FROM qp_t14_rec_t"),
        message
    );

    // Through another table's policy: t's policy reads u, u's policy reads
    // t; the error names the relation whose policies are being expanded
    // when the cycle closes (the statement's target first).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_rec2_app;
         CREATE TABLE qp_t14_rec2_t (id int);
         CREATE TABLE qp_t14_rec2_u (id int);
         GRANT SELECT ON qp_t14_rec2_t, qp_t14_rec2_u TO qp_t14_rec2_app;
         ALTER TABLE qp_t14_rec2_t ENABLE ROW LEVEL SECURITY;
         ALTER TABLE qp_t14_rec2_u ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t14_rec2_tp ON qp_t14_rec2_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t14_rec2_u u
                            WHERE u.id = qp_t14_rec2_t.id));
         CREATE POLICY qp_t14_rec2_up ON qp_t14_rec2_u FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t14_rec2_t t
                            WHERE t.id = qp_t14_rec2_u.id));",
    )]);
    assert_eq!(
        denied(&db, "qp_t14_rec2_app", "SELECT * FROM qp_t14_rec2_t"),
        "infinite recursion detected in policy for relation \"qp_t14_rec2_t\""
    );
    assert_eq!(
        denied(&db, "qp_t14_rec2_app", "SELECT * FROM qp_t14_rec2_u"),
        "infinite recursion detected in policy for relation \"qp_t14_rec2_u\""
    );
    // A policy that reads the other table without a subquery is no cycle.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_rec3_app;
         CREATE TABLE qp_t14_rec3_t (id int);
         CREATE TABLE qp_t14_rec3_u (id int);
         GRANT SELECT ON qp_t14_rec3_t, qp_t14_rec3_u TO qp_t14_rec3_app;
         ALTER TABLE qp_t14_rec3_t ENABLE ROW LEVEL SECURITY;
         ALTER TABLE qp_t14_rec3_u ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t14_rec3_tp ON qp_t14_rec3_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t14_rec3_u u
                            WHERE u.id = qp_t14_rec3_t.id));
         CREATE POLICY qp_t14_rec3_up ON qp_t14_rec3_u FOR SELECT
             USING (qp_t14_rec3_u.id > 0);",
    )]);
    allowed(&db, "qp_t14_rec3_app", "SELECT * FROM qp_t14_rec3_t");

    // A function call in a policy whose body reads the table: the rewriter
    // doesn't expand bodies, so there is no error. (Running it would loop
    // unless it is SECURITY DEFINER, which is the usual way to keep a
    // policy from recursing at execution time.)
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_rec4_app;
         CREATE TABLE qp_t14_rec4_t (id int);
         GRANT SELECT ON qp_t14_rec4_t TO qp_t14_rec4_app;
         ALTER TABLE qp_t14_rec4_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_rec4_guard(int) RETURNS boolean
             LANGUAGE plpgsql STABLE AS $$
             BEGIN PERFORM 1 FROM qp_t14_rec4_t WHERE id = $1; RETURN true; END $$;
         GRANT EXECUTE ON FUNCTION qp_t14_rec4_guard(int) TO qp_t14_rec4_app;
         CREATE POLICY qp_t14_rec4_p ON qp_t14_rec4_t FOR SELECT
             USING (qp_t14_rec4_guard(id));",
    )]);
    allowed(&db, "qp_t14_rec4_app", "SELECT * FROM qp_t14_rec4_t");

    // A view expands into its query: the policy's sublink reads the view as
    // the statement's role, and the view's query reads the table as the
    // view's owner — which recurses exactly when row security applies to
    // that owner too (FORCE).
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_rec5_app;
         CREATE ROLE qp_t14_rec5_owner;
         GRANT USAGE, CREATE ON SCHEMA public TO qp_t14_rec5_owner;
         GRANT USAGE ON SCHEMA public TO qp_t14_rec5_app;
         SET ROLE qp_t14_rec5_owner;
         CREATE TABLE qp_t14_rec5_t (id int);
         GRANT SELECT ON qp_t14_rec5_t TO qp_t14_rec5_app;
         ALTER TABLE qp_t14_rec5_t ENABLE ROW LEVEL SECURITY;
         CREATE VIEW qp_t14_rec5_v AS SELECT * FROM qp_t14_rec5_t;
         RESET ROLE;
         GRANT SELECT ON qp_t14_rec5_v TO qp_t14_rec5_app;
         CREATE POLICY qp_t14_rec5_p ON qp_t14_rec5_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t14_rec5_v));",
    )]);
    allowed(&db, "qp_t14_rec5_app", "SELECT * FROM qp_t14_rec5_t");
    db.apply_sql("ALTER TABLE qp_t14_rec5_t FORCE ROW LEVEL SECURITY;")
        .unwrap();
    assert_eq!(
        denied(&db, "qp_t14_rec5_app", "SELECT * FROM qp_t14_rec5_t"),
        "infinite recursion detected in policy for relation \"qp_t14_rec5_t\""
    );
}

#[test]
fn row_security_policies_follow_renames() {
    // A policy is stored analyzed: PG binds each column to an attnum, each
    // relation and routine to an OID, so `ALTER TABLE ... RENAME [COLUMN]`,
    // `ALTER FUNCTION ... RENAME`, `ALTER ... SET SCHEMA` and `ALTER SCHEMA
    // ... RENAME` leave it meaning what it meant. The analyzer's parse tree
    // follows the objects it resolved when the policy was written. Verified
    // on PostgreSQL 18.7 (`pg_get_expr(polqual, polrelid)` shows the new
    // names).
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_ren_app;
         CREATE SCHEMA qp_t14_ren_s;
         CREATE SCHEMA qp_t14_ren_s2;
         GRANT USAGE ON SCHEMA qp_t14_ren_s, qp_t14_ren_s2 TO qp_t14_ren_app;
         CREATE TABLE qp_t14_ren_s.t (id int, owner_id int);
         CREATE TABLE qp_t14_ren_s.u (id int, owner_id int);
         GRANT SELECT ON qp_t14_ren_s.t, qp_t14_ren_s.u TO qp_t14_ren_app;
         ALTER TABLE qp_t14_ren_s.t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_ren_s.guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t14_ren_s.guard2(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t14_ren_s.guard(int),
             qp_t14_ren_s.guard2(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t14_ren_s.guard2(int) TO qp_t14_ren_app;
         CREATE POLICY qp_t14_ren_p ON qp_t14_ren_s.t FOR SELECT
             USING (qp_t14_ren_s.guard(owner_id)
                    AND EXISTS (SELECT 1 FROM qp_t14_ren_s.u
                                WHERE qp_t14_ren_s.u.owner_id = qp_t14_ren_s.t.owner_id
                                  AND qp_t14_ren_s.guard2(qp_t14_ren_s.u.id)));",
    )]);
    let denied_by = |db: &PgCatalog, relation: &str, function: &str| {
        denied_with(
            db,
            "qp_t14_ren_app",
            &format!("SELECT * FROM {relation}"),
            &format!("permission denied for function {function}"),
        )
    };
    // Only one function is ungranted at a time: with both revoked PG may
    // report either (a multi-error query). Each swap grants before it
    // revokes.
    denied_by(&db, "qp_t14_ren_s.t", "guard");
    db.apply_sql(
        "GRANT EXECUTE ON FUNCTION qp_t14_ren_s.guard(int) TO qp_t14_ren_app;
         REVOKE EXECUTE ON FUNCTION qp_t14_ren_s.guard2(int) FROM qp_t14_ren_app;",
    )
    .unwrap();
    denied_by(&db, "qp_t14_ren_s.t", "guard2");
    db.apply_sql(
        "GRANT EXECUTE ON FUNCTION qp_t14_ren_s.guard2(int) TO qp_t14_ren_app;
         REVOKE EXECUTE ON FUNCTION qp_t14_ren_s.guard(int) FROM qp_t14_ren_app;",
    )
    .unwrap();
    // Column rename: the policy reads the renamed columns, in its own row
    // and in the subquery (both tables have an `owner_id`).
    db.apply_sql("ALTER TABLE qp_t14_ren_s.t RENAME COLUMN owner_id TO owner2;")
        .unwrap();
    denied_by(&db, "qp_t14_ren_s.t", "guard");
    db.apply_sql("ALTER TABLE qp_t14_ren_s.u RENAME COLUMN owner_id TO owner2;")
        .unwrap();
    denied_by(&db, "qp_t14_ren_s.t", "guard");
    // Function rename.
    db.apply_sql("ALTER FUNCTION qp_t14_ren_s.guard(int) RENAME TO guard_r;")
        .unwrap();
    denied_by(&db, "qp_t14_ren_s.t", "guard_r");
    // The renamed function, moved to another schema.
    db.apply_sql("ALTER FUNCTION qp_t14_ren_s.guard_r(int) SET SCHEMA qp_t14_ren_s2;")
        .unwrap();
    denied_by(&db, "qp_t14_ren_s.t", "guard_r");
    // The table and the subquery's table renamed.
    db.apply_sql("ALTER TABLE qp_t14_ren_s.t RENAME TO t2;")
        .unwrap();
    db.apply_sql("ALTER TABLE qp_t14_ren_s.u RENAME TO u2;")
        .unwrap();
    denied_by(&db, "qp_t14_ren_s.t2", "guard_r");
    // A schema rename reaches the subquery's FROM item and its qualifiers.
    db.apply_sql("ALTER SCHEMA qp_t14_ren_s RENAME TO qp_t14_ren_s3;")
        .unwrap();
    denied_by(&db, "qp_t14_ren_s3.t2", "guard_r");
    // And each relation moves to the other schema (the subquery's FROM
    // item and its qualifier follow too).
    db.apply_sql("ALTER TABLE qp_t14_ren_s3.t2 SET SCHEMA qp_t14_ren_s2;")
        .unwrap();
    denied_by(&db, "qp_t14_ren_s2.t2", "guard_r");
    db.apply_sql("ALTER TABLE qp_t14_ren_s3.u2 SET SCHEMA qp_t14_ren_s2;")
        .unwrap();
    denied_by(&db, "qp_t14_ren_s2.t2", "guard_r");
}

#[test]
fn row_security_renames_keep_explicit_aliases() {
    // A relation renamed under an alias that happens to be its old name:
    // the alias is the query's, not the relation's, so a reference through
    // it is not rewritten — only the FROM item is (`FROM old AS old`).
    // Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_als_app;
         CREATE TABLE qp_t14_als_t (id int);
         CREATE TABLE qp_t14_als_u (id int);
         GRANT SELECT ON qp_t14_als_t, qp_t14_als_u TO qp_t14_als_app;
         ALTER TABLE qp_t14_als_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_als_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t14_als_guard(int) FROM PUBLIC;
         CREATE POLICY qp_t14_als_p ON qp_t14_als_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t14_als_u AS qp_t14_als_u
                            WHERE qp_t14_als_u.id = qp_t14_als_t.id
                              AND qp_t14_als_guard(qp_t14_als_u.id)));
         ALTER TABLE qp_t14_als_u RENAME TO qp_t14_als_u2;",
    )]);
    denied_with(
        &db,
        "qp_t14_als_app",
        "SELECT * FROM qp_t14_als_t",
        "permission denied for function qp_t14_als_guard",
    );
}

#[test]
fn row_security_policy_names_stay_bound_through_renames() {
    // The tree follows what its names *meant*, not what they now name: a
    // column renamed away (and its old name given to a column of another
    // type), a function renamed away (its old name given to another
    // function), and a relation renamed away (its old name given to a new
    // relation) leave the policy bound to the original objects, as PG's
    // OIDs do. Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_bnd_app;
         CREATE TABLE qp_t14_bnd_t (a int);
         GRANT SELECT ON qp_t14_bnd_t TO qp_t14_bnd_app;
         ALTER TABLE qp_t14_bnd_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_bnd_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t14_bnd_guard(text) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t14_bnd_guard(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t14_bnd_guard(text) TO qp_t14_bnd_app;
         CREATE POLICY qp_t14_bnd_p ON qp_t14_bnd_t FOR SELECT
             USING (qp_t14_bnd_guard(a));
         ALTER TABLE qp_t14_bnd_t RENAME COLUMN a TO a2;
         ALTER TABLE qp_t14_bnd_t ADD COLUMN a text;",
    )]);
    // The stale name would now call the granted `text` overload.
    denied_with(
        &db,
        "qp_t14_bnd_app",
        "SELECT * FROM qp_t14_bnd_t",
        "permission denied for function qp_t14_bnd_guard",
    );

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_bnd2_app;
         CREATE TABLE qp_t14_bnd2_t (v int);
         GRANT SELECT ON qp_t14_bnd2_t TO qp_t14_bnd2_app;
         ALTER TABLE qp_t14_bnd2_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_bnd2_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t14_bnd2_guard(int) FROM PUBLIC;
         CREATE POLICY qp_t14_bnd2_p ON qp_t14_bnd2_t FOR SELECT
             USING (qp_t14_bnd2_guard(v));
         ALTER FUNCTION qp_t14_bnd2_guard(int) RENAME TO qp_t14_bnd2_guard_r;
         CREATE FUNCTION qp_t14_bnd2_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         GRANT EXECUTE ON FUNCTION qp_t14_bnd2_guard(int) TO qp_t14_bnd2_app;",
    )]);
    // The stale name would now call the new (granted) function; PG calls
    // the renamed one, whose current name the error reports.
    denied_with(
        &db,
        "qp_t14_bnd2_app",
        "SELECT * FROM qp_t14_bnd2_t",
        "permission denied for function qp_t14_bnd2_guard_r",
    );

    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_bnd3_app;
         CREATE TABLE qp_t14_bnd3_t (id int);
         CREATE TABLE qp_t14_bnd3_u (id int);
         GRANT SELECT ON qp_t14_bnd3_t, qp_t14_bnd3_u TO qp_t14_bnd3_app;
         ALTER TABLE qp_t14_bnd3_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_bnd3_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t14_bnd3_guard(int) FROM PUBLIC;
         CREATE POLICY qp_t14_bnd3_p ON qp_t14_bnd3_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t14_bnd3_u
                            WHERE qp_t14_bnd3_u.id = qp_t14_bnd3_t.id
                              AND qp_t14_bnd3_guard(qp_t14_bnd3_u.id)));
         ALTER TABLE qp_t14_bnd3_u RENAME TO qp_t14_bnd3_u2;
         CREATE TABLE qp_t14_bnd3_u (id int);",
    )]);
    // The stale name would now read the new relation, which the role may
    // not select; the policy reads the renamed one.
    db.apply_sql("GRANT SELECT ON qp_t14_bnd3_u2 TO qp_t14_bnd3_app;")
        .unwrap();
    denied_with(
        &db,
        "qp_t14_bnd3_app",
        "SELECT * FROM qp_t14_bnd3_t",
        "permission denied for function qp_t14_bnd3_guard",
    );
}

#[test]
fn row_security_policy_that_cannot_be_analyzed_checks_its_dependencies() {
    // PG keeps a policy analyzed forever; the analyzer re-analyzes its parse
    // tree. When that fails (here: a domain renamed under a cast the
    // analyzer doesn't follow), the policy's `pg_depend` rows still name its
    // routines by OID — checking them keeps the denial. Verified on
    // PostgreSQL 18.7: the domain rename leaves the policy working and the
    // guard's EXECUTE is checked.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_dep_app;
         CREATE DOMAIN qp_t14_dep_color AS text;
         CREATE TABLE qp_t14_dep_t (v qp_t14_dep_color);
         GRANT SELECT ON qp_t14_dep_t TO qp_t14_dep_app;
         ALTER TABLE qp_t14_dep_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_dep_guard(qp_t14_dep_color) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t14_dep_guard(qp_t14_dep_color) FROM PUBLIC;
         CREATE POLICY qp_t14_dep_p ON qp_t14_dep_t FOR SELECT
             USING (qp_t14_dep_guard(v::qp_t14_dep_color));
         ALTER DOMAIN qp_t14_dep_color RENAME TO qp_t14_dep_color2;",
    )]);
    denied_with(
        &db,
        "qp_t14_dep_app",
        "SELECT * FROM qp_t14_dep_t",
        "permission denied for function qp_t14_dep_guard",
    );
    // A relation the policy's subquery scans is still checked too.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_dep2_app;
         CREATE DOMAIN qp_t14_dep2_color AS text;
         CREATE TABLE qp_t14_dep2_t (v qp_t14_dep2_color);
         CREATE TABLE qp_t14_dep2_u (v text);
         GRANT SELECT ON qp_t14_dep2_t TO qp_t14_dep2_app;
         ALTER TABLE qp_t14_dep2_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t14_dep2_p ON qp_t14_dep2_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t14_dep2_u
                            WHERE qp_t14_dep2_u.v = qp_t14_dep2_t.v::text)
                    AND v::qp_t14_dep2_color IS NOT NULL);
         ALTER DOMAIN qp_t14_dep2_color RENAME TO qp_t14_dep2_color2;",
    )]);
    denied_with(
        &db,
        "qp_t14_dep2_app",
        "SELECT * FROM qp_t14_dep2_t",
        "permission denied for table qp_t14_dep2_u",
    );
}

#[test]
fn row_security_insert_select_reads_its_own_target() {
    // `INSERT INTO t SELECT … FROM t` has two RTEs: the target (INSERT's
    // WITH CHECK options, plus SELECT's as checks only when the statement
    // reads the target) and the source scan (SELECT's USING quals as
    // security quals). A restrictive `false` SELECT policy therefore
    // cancels the permissive one in the source scan, but not in the
    // target's WITH CHECK options. Verified on PostgreSQL 18.7.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_ins_app;
         CREATE TABLE qp_t14_ins_t (id int, x int);
         GRANT INSERT, SELECT, UPDATE ON qp_t14_ins_t TO qp_t14_ins_app;
         CREATE UNIQUE INDEX qp_t14_ins_t_id ON qp_t14_ins_t (id);
         ALTER TABLE qp_t14_ins_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_ins_sel(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t14_ins_sel2(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t14_ins_ins(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t14_ins_sel(int),
             qp_t14_ins_sel2(int), qp_t14_ins_ins(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t14_ins_sel(int),
             qp_t14_ins_ins(int) TO qp_t14_ins_app;
         CREATE POLICY qp_t14_ins_psel ON qp_t14_ins_t FOR SELECT
             USING (qp_t14_ins_sel(id));
         CREATE POLICY qp_t14_ins_pfalse ON qp_t14_ins_t AS RESTRICTIVE
             FOR SELECT USING (false);
         CREATE POLICY qp_t14_ins_psel2 ON qp_t14_ins_t FOR SELECT
             USING (qp_t14_ins_sel2(id));
         CREATE POLICY qp_t14_ins_pins ON qp_t14_ins_t FOR INSERT
             WITH CHECK (qp_t14_ins_ins(id));",
    )]);
    // The source scan's security quals fold to `false`: no policy function
    // runs, and the target runs only the INSERT check (SELECT is not
    // required without RETURNING or an arbiter).
    allowed(
        &db,
        "qp_t14_ins_app",
        "INSERT INTO qp_t14_ins_t (id, x) SELECT id, x FROM qp_t14_ins_t",
    );
    allowed(
        &db,
        "qp_t14_ins_app",
        "INSERT INTO qp_t14_ins_t (id, x) SELECT id, x FROM qp_t14_ins_t
         ON CONFLICT DO NOTHING",
    );
    // RETURNING makes the target require SELECT: its SELECT policies become
    // WITH CHECK options, where the restrictive policy is checked on its
    // own and the permissive one still runs.
    denied_with(
        &db,
        "qp_t14_ins_app",
        "INSERT INTO qp_t14_ins_t (id, x) SELECT id, x FROM qp_t14_ins_t
         RETURNING id",
        "permission denied for function qp_t14_ins_sel2",
    );
    // Inferring the arbiter reads the target's column, so SELECT is
    // required too (even with DO NOTHING).
    for sql in [
        "INSERT INTO qp_t14_ins_t (id, x) SELECT id, x FROM qp_t14_ins_t
         ON CONFLICT (id) DO NOTHING",
        "INSERT INTO qp_t14_ins_t (id, x) SELECT id, x FROM qp_t14_ins_t
         ON CONFLICT (id) DO UPDATE SET x = 1",
    ] {
        denied_with(
            &db,
            "qp_t14_ins_app",
            sql,
            "permission denied for function qp_t14_ins_sel2",
        );
    }
    // The INSERT policy is still checked on the target.
    db.apply_sql("REVOKE EXECUTE ON FUNCTION qp_t14_ins_ins(int) FROM qp_t14_ins_app;")
        .unwrap();
    denied_with(
        &db,
        "qp_t14_ins_app",
        "INSERT INTO qp_t14_ins_t (id, x) SELECT id, x FROM qp_t14_ins_t",
        "permission denied for function qp_t14_ins_ins",
    );
}

#[test]
fn row_security_update_select_policies_follow_the_rtes() {
    // `UPDATE t SET …` needs the UPDATE policies; the SELECT ones filter
    // the rows it finds only when it reads a column of the target (a
    // `WHERE`, a `SET` expression, RETURNING). A subquery over the target
    // is an RTE of its own: its SELECT policies are security quals, where a
    // restrictive `false` folds the permissive clause away — unlike the
    // target's own, which get WITH CHECK options too. Verified on
    // PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_upd_app;
         CREATE TABLE qp_t14_upd_t (id int, x int);
         GRANT SELECT, UPDATE ON qp_t14_upd_t TO qp_t14_upd_app;
         ALTER TABLE qp_t14_upd_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_upd_sel(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t14_upd_sel(int) FROM PUBLIC;
         CREATE POLICY qp_t14_upd_psel ON qp_t14_upd_t FOR SELECT
             USING (qp_t14_upd_sel(id));
         CREATE POLICY qp_t14_upd_pfalse ON qp_t14_upd_t AS RESTRICTIVE
             FOR SELECT USING (false);
         CREATE POLICY qp_t14_upd_pupd ON qp_t14_upd_t FOR UPDATE
             USING (true);",
    )]);
    // The subquery's scan folds the SELECT quals to `false`.
    allowed(
        &db,
        "qp_t14_upd_app",
        "UPDATE qp_t14_upd_t SET x = (SELECT max(id) FROM qp_t14_upd_t)",
    );
    // Reading the target's own column applies them to the target (as
    // security quals and WITH CHECK options).
    denied_with(
        &db,
        "qp_t14_upd_app",
        "UPDATE qp_t14_upd_t SET x = 1 WHERE id = 1",
        "permission denied for function qp_t14_upd_sel",
    );
}

#[test]
fn row_security_conflict_update_is_only_an_on_conflict_update() {
    // The auxiliary UPDATE policies are added only for `ON CONFLICT DO
    // UPDATE` (`root->onConflict->action`); an UPDATE in a data-modifying
    // CTE of the same statement is its own statement's target, whose USING
    // clauses are security quals, not forced WITH CHECK options. With a
    // restrictive `false` USING policy, that folds the permissive clause
    // away. Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t14_oc_app;
         CREATE TABLE qp_t14_oc_t (id int, x int);
         GRANT SELECT, INSERT, UPDATE ON qp_t14_oc_t TO qp_t14_oc_app;
         CREATE UNIQUE INDEX qp_t14_oc_t_id ON qp_t14_oc_t (id);
         ALTER TABLE qp_t14_oc_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t14_oc_upd(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t14_oc_upd(int) FROM PUBLIC;
         CREATE POLICY qp_t14_oc_any ON qp_t14_oc_t FOR SELECT USING (true);
         CREATE POLICY qp_t14_oc_ins ON qp_t14_oc_t FOR INSERT WITH CHECK (true);
         CREATE POLICY qp_t14_oc_upd ON qp_t14_oc_t FOR UPDATE
             USING (qp_t14_oc_upd(x)) WITH CHECK (true);
         CREATE POLICY qp_t14_oc_false ON qp_t14_oc_t AS RESTRICTIVE
             FOR UPDATE USING (false);",
    )]);
    allowed(
        &db,
        "qp_t14_oc_app",
        "WITH u AS (UPDATE qp_t14_oc_t SET x = 1)
         INSERT INTO qp_t14_oc_t VALUES (1, 1)",
    );
    denied_with(
        &db,
        "qp_t14_oc_app",
        "INSERT INTO qp_t14_oc_t VALUES (1, 1)
         ON CONFLICT (id) DO UPDATE SET x = 2",
        "permission denied for function qp_t14_oc_upd",
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Tenth round (T15): a realistic RLS review — per-RTE locking, indirection in
// policy expressions, parameter defaults, per-clause checks, writes through a
// view, dummy scans and the denial note
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn row_security_self_scan_lock_applies_update_policies() {
    // `get_row_security_policies` treats every RTE on its own: the DELETE's
    // target applies its DELETE policies, while the `SELECT ... FOR UPDATE`
    // subquery over the same table is an RTE of its own and applies its
    // SELECT and UPDATE policies. The analyzer merges the two occurrences;
    // the lock must still add the UPDATE USING clauses. Verified on
    // PostgreSQL 18.7: `DELETE ... (SELECT ... FOR UPDATE SKIP LOCKED)` and
    // `INSERT INTO t SELECT ... FROM t FOR UPDATE` both raise
    // `permission denied for function f_updu`.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t15_q_app;
         CREATE TABLE qp_t15_q_t (id int PRIMARY KEY, owner text, v int);
         GRANT SELECT, INSERT, UPDATE, DELETE ON qp_t15_q_t TO qp_t15_q_app;
         ALTER TABLE qp_t15_q_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t15_q_updu(text) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t15_q_updu(text) FROM PUBLIC;
         CREATE POLICY qp_t15_q_upd ON qp_t15_q_t FOR UPDATE
             USING (qp_t15_q_updu(owner));
         CREATE POLICY qp_t15_q_sel ON qp_t15_q_t FOR SELECT USING (true);
         CREATE POLICY qp_t15_q_del ON qp_t15_q_t FOR DELETE USING (true);",
    )]);
    for sql in [
        "DELETE FROM qp_t15_q_t
         WHERE id = (SELECT id FROM qp_t15_q_t ORDER BY id
                     FOR UPDATE SKIP LOCKED LIMIT 1)
         RETURNING id",
        "INSERT INTO qp_t15_q_t (id, v)
         SELECT id + 1, v FROM qp_t15_q_t FOR UPDATE",
    ] {
        denied_with(
            &db,
            "qp_t15_q_app",
            sql,
            "permission denied for function qp_t15_q_updu",
        );
    }
    // Without the lock only the SELECT policies apply.
    allowed(
        &db,
        "qp_t15_q_app",
        "DELETE FROM qp_t15_q_t
         WHERE id = (SELECT id FROM qp_t15_q_t ORDER BY id LIMIT 1)",
    );
}

#[test]
fn row_security_policy_calls_under_indirection() {
    // A policy expression stores its parse tree; PG records a `pg_depend`
    // row for every function its analyzed expression calls, including one
    // under a subscript, a slice or a field selection (`A_Indirection`).
    // Verified on PostgreSQL 18.7: each policy denies without EXECUTE on
    // its function, and DROP FUNCTION fails with `policy ... depends on
    // function ...`.
    let base = "CREATE ROLE qp_t15_fold_app;
         CREATE SCHEMA qp_t15_fold_s;
         CREATE TABLE qp_t15_fold_t (name text);
         GRANT SELECT ON qp_t15_fold_t TO qp_t15_fold_app;
         ALTER TABLE qp_t15_fold_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t15_fold_s.fn(text) RETURNS text[]
             LANGUAGE sql IMMUTABLE AS $$ SELECT ARRAY[$1] $$;
         CREATE TYPE qp_t15_fold_pair AS (a text, b text);
         CREATE FUNCTION qp_t15_fold_s.mk(text) RETURNS qp_t15_fold_pair
             LANGUAGE sql IMMUTABLE AS
             $$ SELECT ROW($1, $1)::qp_t15_fold_pair $$;
         REVOKE EXECUTE ON FUNCTION qp_t15_fold_s.fn(text),
             qp_t15_fold_s.mk(text) FROM PUBLIC;";
    for (policy, function, error_name) in [
        (
            "CREATE POLICY qp_t15_fold_p ON qp_t15_fold_t FOR SELECT
                 USING ((qp_t15_fold_s.fn(name))[1] = 'u');",
            "qp_t15_fold_s.fn",
            "fn",
        ),
        (
            "CREATE POLICY qp_t15_fold_p ON qp_t15_fold_t FOR SELECT
                 USING ((qp_t15_fold_s.fn(name))[1:2] = ARRAY['u', 'v']);",
            "qp_t15_fold_s.fn",
            "fn",
        ),
        (
            "CREATE POLICY qp_t15_fold_p ON qp_t15_fold_t FOR SELECT
                 USING ((qp_t15_fold_s.mk(name)).a = 'u');",
            "qp_t15_fold_s.mk",
            "mk",
        ),
    ] {
        let mut db = build_db(&[("0001.sql", &format!("{base}\n{policy}"))]);
        denied_with(
            &db,
            "qp_t15_fold_app",
            "SELECT * FROM qp_t15_fold_t",
            &format!("permission denied for function {error_name}"),
        );
        let error = db
            .apply_sql(&format!("DROP FUNCTION {function}(text)"))
            .expect_err("the policy depends on the function");
        let message = error.to_string();
        assert!(
            message.starts_with(&format!("cannot drop function {function}(text)")),
            "{message}"
        );
        assert!(
            message.contains("policy qp_t15_fold_p on table qp_t15_fold_t depends on function"),
            "{message}"
        );
    }
}

#[test]
fn row_security_policy_parameter_defaults() {
    // `expand_function_arguments` inserts a parameter's DEFAULT into every
    // planned call, stored expressions included: a policy, a column
    // DEFAULT, a CHECK, a generated column, an index expression and a
    // trigger's WHEN all run the default's function. A function the
    // planner inlines and whose body ignores the parameter never evaluates
    // it. Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t15_dflt_app;
         CREATE FUNCTION qp_t15_dflt_sec() RETURNS int
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN 0; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t15_dflt_sec() FROM PUBLIC;
         CREATE FUNCTION qp_t15_dflt_g2(a int, b int DEFAULT qp_t15_dflt_sec())
             RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS $$ BEGIN RETURN a; END $$;
         GRANT EXECUTE ON FUNCTION qp_t15_dflt_g2(int, int) TO qp_t15_dflt_app;
         CREATE FUNCTION qp_t15_dflt_g3(a int, b int DEFAULT qp_t15_dflt_sec())
             RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT $1 $$;
         GRANT EXECUTE ON FUNCTION qp_t15_dflt_g3(int, int) TO qp_t15_dflt_app;
         CREATE FUNCTION qp_t15_dflt_trg() RETURNS trigger
             LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
         GRANT EXECUTE ON FUNCTION qp_t15_dflt_trg() TO qp_t15_dflt_app;
         CREATE TABLE qp_t15_dflt_u (id int);
         GRANT SELECT ON qp_t15_dflt_u TO qp_t15_dflt_app;
         ALTER TABLE qp_t15_dflt_u ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t15_dflt_pu ON qp_t15_dflt_u
             USING (qp_t15_dflt_g2(id) = 0);
         CREATE TABLE qp_t15_dflt_w (id int, ok int DEFAULT qp_t15_dflt_g2(1));
         GRANT INSERT ON qp_t15_dflt_w TO qp_t15_dflt_app;
         CREATE TABLE qp_t15_dflt_w3 (id int CHECK (qp_t15_dflt_g2(id) = 0));
         GRANT INSERT ON qp_t15_dflt_w3 TO qp_t15_dflt_app;
         CREATE TABLE qp_t15_dflt_w4
             (id int, ok int GENERATED ALWAYS AS (qp_t15_dflt_g2(id)) STORED);
         GRANT INSERT ON qp_t15_dflt_w4 TO qp_t15_dflt_app;
         CREATE TABLE qp_t15_dflt_w5 (id int);
         GRANT INSERT ON qp_t15_dflt_w5 TO qp_t15_dflt_app;
         CREATE INDEX qp_t15_dflt_w5_idx ON qp_t15_dflt_w5 (qp_t15_dflt_g2(id));
         CREATE TABLE qp_t15_dflt_w6 (id int);
         GRANT INSERT ON qp_t15_dflt_w6 TO qp_t15_dflt_app;
         CREATE TRIGGER qp_t15_dflt_w6_t BEFORE INSERT ON qp_t15_dflt_w6
             FOR EACH ROW WHEN (qp_t15_dflt_g2(NEW.id) = 0)
             EXECUTE FUNCTION qp_t15_dflt_trg();
         CREATE TABLE qp_t15_dflt_w7 (id int, ok int DEFAULT qp_t15_dflt_g3(1));
         GRANT INSERT ON qp_t15_dflt_w7 TO qp_t15_dflt_app;
         CREATE FUNCTION qp_t15_dflt_stable() RETURNS int
             LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN 0; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t15_dflt_stable() FROM PUBLIC;
         CREATE FUNCTION qp_t15_dflt_gs(a int, b int DEFAULT qp_t15_dflt_stable())
             RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT $1 $$;
         GRANT EXECUTE ON FUNCTION qp_t15_dflt_gs(int, int) TO qp_t15_dflt_app;
         CREATE TABLE qp_t15_dflt_w8 (id int, ok int DEFAULT qp_t15_dflt_gs(1));
         GRANT INSERT ON qp_t15_dflt_w8 TO qp_t15_dflt_app;
         CREATE TABLE qp_t15_dflt_v (id int);
         GRANT SELECT ON qp_t15_dflt_v TO qp_t15_dflt_app;
         ALTER TABLE qp_t15_dflt_v ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t15_dflt_pv ON qp_t15_dflt_v
             USING (qp_t15_dflt_gs(id) = id);",
    )]);
    let message = "permission denied for function qp_t15_dflt_sec";
    // The index expression's insert comes first: once any session has run an
    // insert into the table as a role with EXECUTE (the oracle's own seeding
    // does, as its superuser), PG caches the index expressions for the
    // session and later inserts skip the check. The first execution still
    // denies, which is what the oracle compares.
    for sql in [
        "INSERT INTO qp_t15_dflt_w5 (id) VALUES (1)",
        "SELECT id FROM qp_t15_dflt_u",
        "INSERT INTO qp_t15_dflt_w (id) VALUES (1)",
        "INSERT INTO qp_t15_dflt_w3 (id) VALUES (1)",
        "INSERT INTO qp_t15_dflt_w4 (id) VALUES (1)",
        "INSERT INTO qp_t15_dflt_w6 (id) VALUES (1)",
    ] {
        denied_with(&db, "qp_t15_dflt_app", sql, message);
    }
    // The inlined body doesn't use the parameter, but `eval_const_expressions`
    // simplifies the call's arguments (the DEFAULT among them) before
    // `inline_function` drops it: an IMMUTABLE call over constants there is
    // executed, as the role. Direct calls behave the same.
    for sql in [
        "INSERT INTO qp_t15_dflt_w7 (id) VALUES (1)",
        "SELECT qp_t15_dflt_g3(1)",
        "SELECT qp_t15_dflt_g3(1, qp_t15_dflt_sec())",
        "EXPLAIN SELECT qp_t15_dflt_g3(1)",
    ] {
        denied_with(&db, "qp_t15_dflt_app", sql, message);
    }
    // A STABLE DEFAULT isn't folded: inlining drops it unevaluated.
    for sql in [
        "INSERT INTO qp_t15_dflt_w8 (id) VALUES (1)",
        "SELECT qp_t15_dflt_gs(1)",
        "SELECT qp_t15_dflt_gs(1, qp_t15_dflt_stable())",
        "SELECT id FROM qp_t15_dflt_v",
    ] {
        allowed(&db, "qp_t15_dflt_app", sql);
    }
}

#[test]
fn row_security_policy_clauses_are_checked_per_command() {
    // Only the clause PG builds for the command runs: a read applies the
    // USING clauses, an INSERT the WITH CHECK ones, a locking read the
    // UPDATE USING clauses; a WITH CHECK never runs for a read and a USING
    // never for an INSERT. Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t15_cl_app;
         CREATE ROLE qp_t15_cl_ins;
         CREATE TABLE qp_t15_cl_posts (id int, published bool, author name);
         GRANT SELECT ON qp_t15_cl_posts TO qp_t15_cl_app;
         ALTER TABLE qp_t15_cl_posts ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t15_cl_write() RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t15_cl_write() FROM PUBLIC;
         CREATE POLICY qp_t15_cl_posts_all ON qp_t15_cl_posts
             USING (published OR author = current_user)
             WITH CHECK (author = current_user AND qp_t15_cl_write());
         CREATE TABLE qp_t15_cl_ins (id int);
         GRANT INSERT, SELECT, UPDATE ON qp_t15_cl_ins
             TO qp_t15_cl_ins;
         ALTER TABLE qp_t15_cl_ins ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t15_cl_fu() RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t15_cl_fc() RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t15_cl_fu() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t15_cl_fc() TO qp_t15_cl_ins;
         CREATE POLICY qp_t15_cl_ins_all ON qp_t15_cl_ins FOR ALL
             USING (qp_t15_cl_fu()) WITH CHECK (qp_t15_cl_fc());
         CREATE TABLE qp_t15_cl_lock (id int);
         GRANT SELECT, UPDATE ON qp_t15_cl_lock TO qp_t15_cl_ins;
         ALTER TABLE qp_t15_cl_lock ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t15_cl_lsel(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t15_cl_lupd(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         CREATE FUNCTION qp_t15_cl_lchk(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t15_cl_lsel(int),
             qp_t15_cl_lupd(int), qp_t15_cl_lchk(int) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION qp_t15_cl_lsel(int),
             qp_t15_cl_lupd(int) TO qp_t15_cl_ins;
         CREATE POLICY qp_t15_cl_lsel ON qp_t15_cl_lock FOR SELECT
             USING (qp_t15_cl_lsel(id));
         CREATE POLICY qp_t15_cl_lupd ON qp_t15_cl_lock FOR UPDATE
             USING (qp_t15_cl_lupd(id)) WITH CHECK (qp_t15_cl_lchk(id));",
    )]);
    // The read never builds the WITH CHECK clause.
    allowed(
        &db,
        "qp_t15_cl_app",
        "SELECT id FROM qp_t15_cl_posts WHERE published",
    );
    allowed(
        &db,
        "qp_t15_cl_app",
        "EXPLAIN SELECT id FROM qp_t15_cl_posts WHERE published",
    );
    // ... but the INSERT checks only WITH CHECK, never USING.
    allowed(
        &db,
        "qp_t15_cl_ins",
        "INSERT INTO qp_t15_cl_ins (id) VALUES (1)",
    );
    // A locking read applies the UPDATE USING clauses, not their WITH CHECK.
    allowed(
        &db,
        "qp_t15_cl_ins",
        "SELECT id FROM qp_t15_cl_lock FOR UPDATE",
    );
    allowed(
        &db,
        "qp_t15_cl_ins",
        "SELECT id FROM qp_t15_cl_lock FOR SHARE",
    );
    // An UPDATE runs both.
    denied_with(
        &db,
        "qp_t15_cl_ins",
        "UPDATE qp_t15_cl_lock SET id = id",
        "permission denied for function qp_t15_cl_lchk",
    );
}

#[test]
fn row_security_insert_through_view_selects_as_the_owner() {
    // `rewriteTargetView` makes the base relation the statement's target:
    // a plain INSERT runs only its INSERT policies. RETURNING and
    // `ON CONFLICT` also need SELECT on the target and so apply its SELECT
    // policies — checked as the view's owner (`checkAsUser`), not as the
    // caller. Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t15_vw_app;
         CREATE ROLE qp_t15_vw_owner;
         CREATE TABLE qp_t15_vw_items (id int PRIMARY KEY, tenant int NOT NULL);
         CREATE TABLE qp_t15_vw_tu2 (tenant int);
         ALTER TABLE qp_t15_vw_items ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t15_vw_sel ON qp_t15_vw_items FOR SELECT
             USING (tenant IN (SELECT tenant FROM qp_t15_vw_tu2));
         CREATE POLICY qp_t15_vw_ins ON qp_t15_vw_items FOR INSERT
             WITH CHECK (true);
         GRANT SELECT, INSERT ON qp_t15_vw_items TO qp_t15_vw_owner;
         CREATE VIEW qp_t15_vw_v AS SELECT id, tenant FROM qp_t15_vw_items;
         ALTER VIEW qp_t15_vw_v OWNER TO qp_t15_vw_owner;
         GRANT SELECT, INSERT ON qp_t15_vw_v TO qp_t15_vw_app;",
    )]);
    // No SELECT policies are applied: the owner's missing SELECT on tu2
    // never comes up.
    allowed(
        &db,
        "qp_t15_vw_app",
        "INSERT INTO qp_t15_vw_v (id, tenant) VALUES (1, 1)",
    );
    for sql in [
        "INSERT INTO qp_t15_vw_v (id, tenant) VALUES (2, 1) RETURNING id",
        "INSERT INTO qp_t15_vw_v (id, tenant) VALUES (3, 1)
         ON CONFLICT (id) DO NOTHING",
    ] {
        let e = denied_with(
            &db,
            "qp_t15_vw_app",
            sql,
            "permission denied for table qp_t15_vw_tu2",
        );
        assert!(e.contains("being the view's owner"), "{e}");
    }
    // The caller reads the view: the owner's SELECT policies apply too.
    denied_with(
        &db,
        "qp_t15_vw_app",
        "SELECT id FROM qp_t15_vw_v",
        "permission denied for table qp_t15_vw_tu2",
    );
}

#[test]
fn row_security_dummy_scan_builds_no_policy_quals() {
    // A scan the planner proves empty (a constant-FALSE or NULL WHERE, or an
    // inner join with a false ON) is replaced by a dummy relation: its
    // security quals are never built, so its policy functions don't run.
    // The rest of the statement still is: a function in the target list
    // is initialized with the plan. Verified on PostgreSQL 18.7.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t15_dmy_app;
         CREATE TABLE qp_t15_dmy_a (id int, tenant int);
         CREATE TABLE qp_t15_dmy_b (id int, tenant int);
         GRANT SELECT ON qp_t15_dmy_a, qp_t15_dmy_b TO qp_t15_dmy_app;
         ALTER TABLE qp_t15_dmy_a ENABLE ROW LEVEL SECURITY;
         ALTER TABLE qp_t15_dmy_b ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t15_dmy_f(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t15_dmy_f(int) FROM PUBLIC;
         CREATE POLICY qp_t15_dmy_fa ON qp_t15_dmy_a FOR SELECT
             USING (qp_t15_dmy_f(tenant));
         CREATE POLICY qp_t15_dmy_fb ON qp_t15_dmy_b FOR SELECT
             USING (qp_t15_dmy_f(tenant));",
    )]);
    for sql in [
        "SELECT id FROM qp_t15_dmy_a WHERE false",
        "SELECT id FROM qp_t15_dmy_a WHERE 1 = 0",
        "SELECT id FROM qp_t15_dmy_a WHERE NULL",
        "SELECT id FROM qp_t15_dmy_a WHERE false LIMIT 0",
        "SELECT a.id FROM qp_t15_dmy_a a JOIN qp_t15_dmy_b b ON false",
        "SELECT a.id FROM qp_t15_dmy_a a, qp_t15_dmy_b b WHERE false",
        "SELECT a.id FROM qp_t15_dmy_a a
         JOIN qp_t15_dmy_b b ON a.id = b.id AND false",
    ] {
        allowed(&db, "qp_t15_dmy_app", sql);
    }
    // The relation is scanned without the qual: the policy runs.
    denied_with(
        &db,
        "qp_t15_dmy_app",
        "SELECT id FROM qp_t15_dmy_a WHERE true",
        "permission denied for function qp_t15_dmy_f",
    );
    // An outer join keeps the left side scanned.
    denied_with(
        &db,
        "qp_t15_dmy_app",
        "SELECT a.id FROM qp_t15_dmy_a a
         LEFT JOIN qp_t15_dmy_b b ON false",
        "permission denied for function qp_t15_dmy_f",
    );
    // A function in the target list is initialized with the plan.
    denied_with(
        &db,
        "qp_t15_dmy_app",
        "SELECT qp_t15_dmy_f(id) FROM qp_t15_dmy_a WHERE false",
        "permission denied for function qp_t15_dmy_f",
    );
}

#[test]
fn row_security_denial_note_names_the_view_only_when_one() {
    // `checkAsUser` is a view's owner when the policy applies to a table the
    // view reads; a policy's subquery relations always take that role, and
    // the denial note must only call it the view's owner when a view
    // actually caused it. Verified on PostgreSQL 18.7 (the message is the
    // analyzer's own; PG only reports `permission denied for table ...`).
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_t15_note_app;
         CREATE ROLE qp_t15_note_owner;
         CREATE TABLE qp_t15_note_t (id int);
         CREATE TABLE qp_t15_note_u (id int);
         GRANT SELECT ON qp_t15_note_t TO qp_t15_note_app;
         ALTER TABLE qp_t15_note_t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY qp_t15_note_p ON qp_t15_note_t FOR SELECT
             USING (EXISTS (SELECT 1 FROM qp_t15_note_u
                            WHERE qp_t15_note_u.id = qp_t15_note_t.id));
         CREATE VIEW qp_t15_note_v AS SELECT id FROM qp_t15_note_t;
         ALTER VIEW qp_t15_note_v OWNER TO qp_t15_note_owner;
         GRANT SELECT ON qp_t15_note_v TO qp_t15_note_app;
         GRANT SELECT ON qp_t15_note_t TO qp_t15_note_owner;
         GRANT SELECT ON qp_t15_note_u TO qp_t15_note_owner;",
    )]);
    let direct = denied_with(
        &db,
        "qp_t15_note_app",
        "SELECT id FROM qp_t15_note_t",
        "permission denied for table qp_t15_note_u",
    );
    assert!(!direct.contains("view's query"), "{direct}");
    db.apply_sql("REVOKE SELECT ON qp_t15_note_u FROM qp_t15_note_owner;")
        .unwrap();
    let through = denied_with(
        &db,
        "qp_t15_note_app",
        "SELECT id FROM qp_t15_note_v",
        "permission denied for table qp_t15_note_u",
    );
    assert!(through.contains("being the view's owner"), "{through}");
}

#[test]
fn row_security_policy_clauses_follow_renamed_objects() {
    // A stored policy names its types, collations, operators and
    // column-notation calls; PG keeps them by OID, so renaming the object or
    // moving it to another schema leaves the policy working. The analyzer
    // rewrites what it recorded, and a column-notation call (which has no
    // syntax to qualify) resolves by the routine's OID. Verified on
    // PostgreSQL 18.7: each policy still folds, so the guard in the dead arm
    // is not checked. Every section has its own role (the oracle shares one
    // cluster per test, and roles are cluster-wide).
    let base = "
         CREATE ROLE qp_t15_obj_app;
         CREATE TABLE qp_t15_obj_t (id int, name text);
         GRANT SELECT ON qp_t15_obj_t TO qp_t15_obj_app;
         ALTER TABLE qp_t15_obj_t ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION qp_t15_obj_guard(int) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
         REVOKE EXECUTE ON FUNCTION qp_t15_obj_guard(int) FROM PUBLIC;";

    // A domain's RENAME and SET SCHEMA.
    let mut db = build_db(&[(
        "0001.sql",
        &format!(
            "{}
             CREATE SCHEMA qp_t15_o1_s;
             CREATE SCHEMA qp_t15_o1_s2;
             CREATE DOMAIN qp_t15_o1_s.dom AS text;
             CREATE POLICY qp_t15_o1_p ON qp_t15_o1_t FOR SELECT
                 USING (true OR (qp_t15_o1_guard(id)
                                 AND name::qp_t15_o1_s.dom = 'x'));",
            base.replace("qp_t15_obj_", "qp_t15_o1_")
        ),
    )]);
    allowed(&db, "qp_t15_o1_app", "SELECT id FROM qp_t15_o1_t");
    db.apply_sql("ALTER DOMAIN qp_t15_o1_s.dom RENAME TO dom2;")
        .unwrap();
    allowed(&db, "qp_t15_o1_app", "SELECT id FROM qp_t15_o1_t");
    db.apply_sql("ALTER DOMAIN qp_t15_o1_s.dom2 SET SCHEMA qp_t15_o1_s2;")
        .unwrap();
    allowed(&db, "qp_t15_o1_app", "SELECT id FROM qp_t15_o1_t");
    // With the object still named correctly the guard is live and checked.
    db.apply_sql("ALTER POLICY qp_t15_o1_p ON qp_t15_o1_t USING (qp_t15_o1_guard(id));")
        .unwrap();
    denied_with(
        &db,
        "qp_t15_o1_app",
        "SELECT id FROM qp_t15_o1_t",
        "permission denied for function qp_t15_o1_guard",
    );

    // A collation's RENAME and SET SCHEMA.
    let mut db = build_db(&[(
        "0001.sql",
        &format!(
            "{}
             CREATE SCHEMA qp_t15_o2_s;
             CREATE SCHEMA qp_t15_o2_s2;
             CREATE COLLATION qp_t15_o2_s.col (provider = libc, locale = 'C');
             CREATE POLICY qp_t15_o2_p ON qp_t15_o2_t FOR SELECT
                 USING (true OR (qp_t15_o2_guard(id)
                                 AND name COLLATE qp_t15_o2_s.col = 'x'));",
            base.replace("qp_t15_obj_", "qp_t15_o2_")
        ),
    )]);
    allowed(&db, "qp_t15_o2_app", "SELECT id FROM qp_t15_o2_t");
    db.apply_sql("ALTER COLLATION qp_t15_o2_s.col RENAME TO col2;")
        .unwrap();
    allowed(&db, "qp_t15_o2_app", "SELECT id FROM qp_t15_o2_t");
    db.apply_sql("ALTER COLLATION qp_t15_o2_s.col2 SET SCHEMA qp_t15_o2_s2;")
        .unwrap();
    allowed(&db, "qp_t15_o2_app", "SELECT id FROM qp_t15_o2_t");

    // An operator's SET SCHEMA (it has no RENAME) and its function's EXECUTE.
    let mut db = build_db(&[(
        "0001.sql",
        &format!(
            "{}
             CREATE SCHEMA qp_t15_o3_s;
             CREATE SCHEMA qp_t15_o3_s2;
             CREATE FUNCTION qp_t15_o3_eq(int, int) RETURNS boolean
                 LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
             REVOKE EXECUTE ON FUNCTION qp_t15_o3_eq(int, int) FROM PUBLIC;
             CREATE OPERATOR qp_t15_o3_s.?# (LEFTARG = int, RIGHTARG = int,
                 FUNCTION = qp_t15_o3_eq);
             CREATE POLICY qp_t15_o3_p ON qp_t15_o3_t FOR SELECT
                 USING (true OR (qp_t15_o3_guard(id)
                                 AND id OPERATOR(qp_t15_o3_s.?#) 1));",
            base.replace("qp_t15_obj_", "qp_t15_o3_")
        ),
    )]);
    allowed(&db, "qp_t15_o3_app", "SELECT id FROM qp_t15_o3_t");
    db.apply_sql("ALTER OPERATOR qp_t15_o3_s.?# (int, int) SET SCHEMA qp_t15_o3_s2;")
        .unwrap();
    allowed(&db, "qp_t15_o3_app", "SELECT id FROM qp_t15_o3_t");
    db.apply_sql("ALTER SCHEMA qp_t15_o3_s2 RENAME TO qp_t15_o3_s3;")
        .unwrap();
    allowed(&db, "qp_t15_o3_app", "SELECT id FROM qp_t15_o3_t");
    // The live policy runs the operator, whose function needs EXECUTE.
    db.apply_sql(
        "GRANT EXECUTE ON FUNCTION qp_t15_o3_guard(int) TO qp_t15_o3_app;
         ALTER POLICY qp_t15_o3_p ON qp_t15_o3_t
             USING (id OPERATOR(qp_t15_o3_s3.?#) 1);",
    )
    .unwrap();
    denied_with(
        &db,
        "qp_t15_o3_app",
        "SELECT id FROM qp_t15_o3_t",
        "permission denied for function qp_t15_o3_eq",
    );

    // A column-notation call: `(row).f` is `f(row)` in PG, and neither a
    // rename nor a SET SCHEMA of `f` has syntax to follow in `(row).f`.
    let mut db = build_db(&[(
        "0001.sql",
        &format!(
            "{}
             CREATE SCHEMA qp_t15_o4_s2;
             CREATE FUNCTION qp_t15_o4_row_f(qp_t15_o4_t) RETURNS boolean
                 LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN true; END $$;
             REVOKE EXECUTE ON FUNCTION qp_t15_o4_row_f(qp_t15_o4_t) FROM PUBLIC;
             CREATE POLICY qp_t15_o4_p ON qp_t15_o4_t FOR SELECT
                 USING (true OR (qp_t15_o4_guard(id)
                                 AND (qp_t15_o4_t).qp_t15_o4_row_f));",
            base.replace("qp_t15_obj_", "qp_t15_o4_")
        ),
    )]);
    allowed(&db, "qp_t15_o4_app", "SELECT id FROM qp_t15_o4_t");
    db.apply_sql("ALTER FUNCTION qp_t15_o4_row_f(qp_t15_o4_t) RENAME TO qp_t15_o4_row_f2;")
        .unwrap();
    allowed(&db, "qp_t15_o4_app", "SELECT id FROM qp_t15_o4_t");
    db.apply_sql("ALTER FUNCTION qp_t15_o4_row_f2(qp_t15_o4_t) SET SCHEMA qp_t15_o4_s2;")
        .unwrap();
    allowed(&db, "qp_t15_o4_app", "SELECT id FROM qp_t15_o4_t");
}

// ──────────────────────────────────────────────────────────────────────────────
// Utility statements: TRUNCATE, LOCK, ANALYZE, REFRESH MATERIALIZED VIEW, SET.
// They check their own privileges when they execute (verified on 18.7).
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn truncate_needs_truncate_on_what_it_names() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_ut_app;
         CREATE TABLE qp_ut_t (id int);
         CREATE TABLE qp_ut_p (id int);
         CREATE TABLE qp_ut_pc () INHERITS (qp_ut_p);
         CREATE TABLE qp_ut_ref (id int PRIMARY KEY);
         CREATE TABLE qp_ut_refby (id int REFERENCES qp_ut_ref);
         CREATE TABLE qp_ut_idt (id int GENERATED BY DEFAULT AS IDENTITY, s serial);
         GRANT TRUNCATE ON qp_ut_p, qp_ut_ref, qp_ut_idt TO qp_ut_app;",
    )]);
    denied_with(
        &db,
        "qp_ut_app",
        "TRUNCATE qp_ut_t",
        "permission denied for table qp_ut_t",
    );
    // The children of an inheritance tree need no privilege of their own.
    allowed(&db, "qp_ut_app", "TRUNCATE qp_ut_p");
    // CASCADE truncates the referencing tables, each checked.
    denied_with(
        &db,
        "qp_ut_app",
        "TRUNCATE qp_ut_ref CASCADE",
        "permission denied for table qp_ut_refby",
    );
    allowed(&db, "qp_ut_app", "TRUNCATE qp_ut_idt");
    // RESTART IDENTITY resets the owned sequences: only their owner may.
    denied_with(
        &db,
        "qp_ut_app",
        "TRUNCATE qp_ut_idt RESTART IDENTITY",
        "must be owner of sequence qp_ut_idt_",
    );
}

#[test]
fn truncate_fires_its_statement_triggers() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_ut_trg_app;
         CREATE TABLE qp_ut_trg_secret (id int);
         CREATE TABLE qp_ut_trg_t (id int);
         GRANT TRUNCATE ON qp_ut_trg_t TO qp_ut_trg_app;
         CREATE FUNCTION qp_ut_trg_f() RETURNS trigger LANGUAGE plpgsql AS
             $$ BEGIN PERFORM 1 FROM qp_ut_trg_secret; RETURN NULL; END $$;
         CREATE TRIGGER qp_ut_trg AFTER TRUNCATE ON qp_ut_trg_t
             FOR EACH STATEMENT EXECUTE FUNCTION qp_ut_trg_f();",
    )]);
    denied_with(
        &db,
        "qp_ut_trg_app",
        "TRUNCATE qp_ut_trg_t",
        "permission denied for table qp_ut_trg_secret",
    );
}

#[test]
fn lock_needs_a_privilege_the_mode_accepts() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_ul_app;
         CREATE ROLE qp_ul_owner;
         CREATE TABLE qp_ul_t (id int);
         CREATE TABLE qp_ul_i (id int);
         CREATE TABLE qp_ul_u (id int);
         CREATE TABLE qp_ul_base (id int);
         GRANT SELECT ON qp_ul_t TO qp_ul_app;
         GRANT INSERT ON qp_ul_i TO qp_ul_app;
         GRANT UPDATE ON qp_ul_u TO qp_ul_app;
         CREATE VIEW qp_ul_v AS SELECT * FROM qp_ul_base;
         ALTER VIEW qp_ul_v OWNER TO qp_ul_owner;
         GRANT SELECT ON qp_ul_v TO qp_ul_app;",
    )]);
    allowed(&db, "qp_ul_app", "LOCK qp_ul_t IN ACCESS SHARE MODE");
    denied_with(
        &db,
        "qp_ul_app",
        "LOCK qp_ul_t IN ROW SHARE MODE",
        "permission denied for table qp_ul_t",
    );
    allowed(&db, "qp_ul_app", "LOCK qp_ul_i IN ROW EXCLUSIVE MODE");
    denied_with(
        &db,
        "qp_ul_app",
        "LOCK qp_ul_i",
        "permission denied for table qp_ul_i",
    );
    allowed(&db, "qp_ul_app", "LOCK qp_ul_u");
    // A view's tables are locked too, checked as the view's owner.
    denied_with(
        &db,
        "qp_ul_app",
        "LOCK qp_ul_v IN ACCESS SHARE MODE",
        "permission denied for table qp_ul_base",
    );
}

#[test]
fn analyze_skips_what_the_role_may_not_maintain() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_ua_app;
         CREATE TABLE qp_ua_t (id int);",
    )]);
    // `permission denied to analyze "qp_ua_t", skipping it` is a WARNING.
    allowed(&db, "qp_ua_app", "ANALYZE qp_ua_t");
}

#[test]
fn refresh_needs_maintain_and_runs_the_query_as_the_owner() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_ur_app;
         CREATE ROLE qp_ur_owner;
         CREATE TABLE qp_ur_t (id int);
         CREATE MATERIALIZED VIEW qp_ur_mv AS SELECT id FROM qp_ur_t;
         CREATE MATERIALIZED VIEW qp_ur_mv2 AS SELECT id FROM qp_ur_t;
         ALTER MATERIALIZED VIEW qp_ur_mv2 OWNER TO qp_ur_owner;
         GRANT MAINTAIN ON qp_ur_mv, qp_ur_mv2 TO qp_ur_app;
         CREATE MATERIALIZED VIEW qp_ur_mv3 AS SELECT id FROM qp_ur_t;",
    )]);
    allowed(&db, "qp_ur_app", "REFRESH MATERIALIZED VIEW qp_ur_mv");
    denied_with(
        &db,
        "qp_ur_app",
        "REFRESH MATERIALIZED VIEW qp_ur_mv3",
        "permission denied for materialized view qp_ur_mv3",
    );
    denied_with(
        &db,
        "qp_ur_app",
        "REFRESH MATERIALIZED VIEW qp_ur_mv2",
        "permission denied for table qp_ur_t",
    );
    // WITH NO DATA runs no query.
    allowed(
        &db,
        "qp_ur_app",
        "REFRESH MATERIALIZED VIEW qp_ur_mv2 WITH NO DATA",
    );
}

#[test]
fn set_of_a_superuser_parameter_needs_set_on_it() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_us_app;
         CREATE ROLE qp_us_granted;
         GRANT SET ON PARAMETER log_min_duration_statement TO qp_us_granted;",
    )]);
    allowed(&db, "qp_us_app", "SET statement_timeout = '1s'");
    denied_with(
        &db,
        "qp_us_app",
        "SET log_min_duration_statement = 1",
        "permission denied to set parameter \"log_min_duration_statement\"",
    );
    denied_with(
        &db,
        "qp_us_app",
        "RESET log_min_duration_statement",
        "permission denied to set parameter \"log_min_duration_statement\"",
    );
    allowed(
        &db,
        "qp_us_granted",
        "SET LOCAL log_min_duration_statement = 1",
    );
}

#[test]
fn set_config_of_a_superuser_parameter_needs_set_on_it() {
    // set_config_by_name checks it when the call runs.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_usc_app;
         CREATE ROLE qp_usc_granted;
         GRANT SET ON PARAMETER log_min_duration_statement TO qp_usc_granted;",
    )]);
    allowed(
        &db,
        "qp_usc_app",
        "SELECT set_config('statement_timeout', '1s', true)",
    );
    for sql in [
        "SELECT set_config('log_min_duration_statement', '1', false)",
        "SELECT set_config('Log_Min_Duration_Statement', '1', true)",
        "SELECT 1 WHERE set_config('log_min_duration_statement', '1', true) IS NULL",
    ] {
        denied_with(
            &db,
            "qp_usc_app",
            sql,
            "permission denied to set parameter \"log_min_duration_statement\"",
        );
    }
    allowed(
        &db,
        "qp_usc_granted",
        "SELECT set_config('log_min_duration_statement', '1', true)",
    );
    // Nothing runs it: an EXPLAIN without ANALYZE, an unreached CASE result.
    allowed(
        &db,
        "qp_usc_app",
        "EXPLAIN SELECT set_config('log_min_duration_statement', '1', false)",
    );
    allowed(
        &db,
        "qp_usc_app",
        "SELECT CASE WHEN false THEN set_config('log_min_duration_statement', '1', true) END",
    );
}

#[test]
fn analyze_of_a_foreign_table_without_a_handler_fails_for_a_role_that_may_analyze_it() {
    // vacuum_is_permitted_for_relation skips (with a WARNING) a relation
    // the role has no MAINTAIN on; analyze_rel then opens the wrapper's
    // handler of the table and of the inheritors it samples.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_afh_app;
         CREATE ROLE qp_afh_maint;
         CREATE FOREIGN DATA WRAPPER qp_afh_w;
         CREATE SERVER qp_afh_s FOREIGN DATA WRAPPER qp_afh_w;
         CREATE FOREIGN TABLE qp_afh_ft (a int) SERVER qp_afh_s;
         CREATE TABLE qp_afh_ip (a int);
         CREATE FOREIGN TABLE qp_afh_ic () INHERITS (qp_afh_ip) SERVER qp_afh_s;
         CREATE TABLE qp_afh_t (a int);
         GRANT MAINTAIN ON qp_afh_ft, qp_afh_ip, qp_afh_t TO qp_afh_maint;",
    )]);
    // Inheritance statistics sample the children, ONLY or not.
    for sql in [
        "ANALYZE qp_afh_ft",
        "ANALYZE qp_afh_ip",
        "ANALYZE ONLY qp_afh_ip",
    ] {
        allowed(&db, "qp_afh_app", sql);
        denied_with(
            &db,
            "qp_afh_maint",
            sql,
            "foreign-data wrapper \"qp_afh_w\" has no handler",
        );
    }
    allowed(&db, "qp_afh_maint", "ANALYZE qp_afh_t");
}

/// The schema and roles of the `copy_in!` role checks: what PG 19.0 does
/// for each was checked by running the COPY as the role.
const COPY_IN_SETUP: &str = "CREATE ROLE qp_ci_app;
     CREATE ROLE qp_ci_cols;
     CREATE ROLE qp_ci_off;
     ALTER ROLE qp_ci_off SET row_security = off;
     CREATE TABLE qp_ci_t (a int, b text);
     GRANT INSERT ON qp_ci_t TO qp_ci_app;
     GRANT INSERT (a) ON qp_ci_t TO qp_ci_cols;
     CREATE TABLE qp_ci_rls (a int);
     ALTER TABLE qp_ci_rls ENABLE ROW LEVEL SECURITY;
     CREATE POLICY qp_ci_all ON qp_ci_rls USING (true) WITH CHECK (true);
     GRANT INSERT ON qp_ci_rls TO qp_ci_app, qp_ci_off;
     CREATE TABLE qp_ci_log (a int);
     CREATE FUNCTION qp_ci_trg() RETURNS trigger LANGUAGE plpgsql
         AS $$ BEGIN INSERT INTO qp_ci_log VALUES (NEW.a); RETURN NEW; END $$;
     CREATE TABLE qp_ci_trig (a int);
     CREATE TRIGGER qp_ci_ti BEFORE INSERT ON qp_ci_trig
         FOR EACH ROW EXECUTE FUNCTION qp_ci_trg();
     GRANT INSERT ON qp_ci_trig TO qp_ci_app;
     CREATE SEQUENCE qp_ci_seq;
     CREATE TABLE qp_ci_def (id bigint DEFAULT nextval('qp_ci_seq'), a int);
     GRANT INSERT ON qp_ci_def TO qp_ci_app;
     CREATE TABLE qp_ci_id (id int GENERATED ALWAYS AS IDENTITY, a int);
     GRANT INSERT ON qp_ci_id TO qp_ci_app;
     CREATE VIEW qp_ci_v AS SELECT a FROM qp_ci_log;
     CREATE TRIGGER qp_ci_vi INSTEAD OF INSERT ON qp_ci_v
         FOR EACH ROW EXECUTE FUNCTION qp_ci_trg();
     GRANT INSERT ON qp_ci_v TO qp_ci_app;";

#[track_caller]
fn copy_in_denied(db: &PgCatalog, role: &str, target: &str, prefix: &str) {
    match db.analyze_copy_in_as(target, role) {
        Ok(_) => panic!("expected {role} to be denied COPY {target} FROM STDIN"),
        Err(e) => assert!(
            e.to_string().starts_with(prefix),
            "{role} copying into {target}\n  expected prefix: {prefix}\n             got: {e}"
        ),
    }
}

#[track_caller]
fn copy_in_allowed(db: &PgCatalog, role: &str, target: &str) {
    if let Err(e) = db.analyze_copy_in_as(target, role) {
        panic!("expected {role} to be allowed COPY {target} FROM STDIN\n{e}");
    }
}

#[test]
fn copy_in_needs_insert_on_the_columns_it_copies() {
    let db = build_db(&[("0001.sql", COPY_IN_SETUP)]);
    copy_in_allowed(&db, "qp_ci_app", "qp_ci_t");
    copy_in_allowed(&db, "qp_ci_cols", "qp_ci_t (a)");
    copy_in_denied(
        &db,
        "qp_ci_cols",
        "qp_ci_t (a, b)",
        "permission denied for table qp_ci_t",
    );
    copy_in_denied(
        &db,
        "qp_ci_cols",
        "qp_ci_t",
        "permission denied for table qp_ci_t",
    );
    copy_in_denied(
        &db,
        "qp_ci_cols",
        "qp_ci_log",
        "permission denied for table qp_ci_log",
    );
    let err = db.analyze_copy_in_as("qp_ci_t", "nobody").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("role \"nobody\" does not exist"),
        "{err}"
    );
}

#[test]
fn copy_in_refuses_row_level_security() {
    // DoCopy: ExecCheckPermissions, then check_enable_rls — which raises
    // itself under row_security off.
    let db = build_db(&[("0001.sql", COPY_IN_SETUP)]);
    copy_in_denied(
        &db,
        "qp_ci_app",
        "qp_ci_rls",
        "COPY FROM not supported with row-level security",
    );
    copy_in_denied(
        &db,
        "qp_ci_off",
        "qp_ci_rls",
        "query would be affected by row-level security policy for table \"qp_ci_rls\"",
    );
    copy_in_denied(
        &db,
        "qp_ci_cols",
        "qp_ci_rls",
        "permission denied for table qp_ci_rls",
    );
}

#[test]
fn copy_in_runs_the_triggers_and_defaults_of_an_insert() {
    let db = build_db(&[("0001.sql", COPY_IN_SETUP)]);
    // The trigger function runs as the role, and inserts into qp_ci_log.
    copy_in_denied(
        &db,
        "qp_ci_app",
        "qp_ci_trig",
        "permission denied for table qp_ci_log",
    );
    copy_in_denied(
        &db,
        "qp_ci_app",
        "qp_ci_v (a)",
        "permission denied for table qp_ci_log",
    );
    // A column it leaves to its default runs nextval.
    copy_in_denied(
        &db,
        "qp_ci_app",
        "qp_ci_def (a)",
        "permission denied for sequence qp_ci_seq",
    );
    copy_in_allowed(&db, "qp_ci_app", "qp_ci_def (id, a)");
    // An identity column's sequence isn't checked.
    copy_in_allowed(&db, "qp_ci_app", "qp_ci_id (a)");
}

#[test]
fn set_role_needs_membership_with_set() {
    // The mirror runs as a superuser session under SET ROLE, where SET ROLE
    // to anything succeeds: an application role logging in as itself is
    // what the check models.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE ROLE qp_usr_app;
         CREATE ROLE qp_usr_admin;
         CREATE ROLE qp_usr_reader;
         GRANT qp_usr_reader TO qp_usr_app;
         GRANT qp_usr_admin TO qp_usr_app WITH SET FALSE;",
    )]);
    db.skip_pg_sanity();
    allowed(&db, "qp_usr_app", "SET ROLE qp_usr_reader");
    allowed(&db, "qp_usr_app", "RESET ROLE");
    denied_with(
        &db,
        "qp_usr_app",
        "SET ROLE qp_usr_admin",
        "permission denied to set role \"qp_usr_admin\"",
    );
    denied_with(
        &db,
        "qp_usr_app",
        "SET SESSION AUTHORIZATION qp_usr_reader",
        "permission denied to set session authorization \"qp_usr_reader\"",
    );
}
