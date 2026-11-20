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
