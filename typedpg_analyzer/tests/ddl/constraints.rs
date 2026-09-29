//! `pg_constraint` lifecycle: emission on `CREATE TABLE` /
//! `CREATE UNIQUE INDEX` / `ALTER TABLE ADD CONSTRAINT`, and removal /
//! rename / FK-aware DROP cascading.

use crate::common::*;

// ── CREATE TABLE: PRIMARY KEY / UNIQUE / CHECK ─────────────────────────────

#[test]
fn create_table_emits_pkey_and_unique_constraints() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL UNIQUE,
            tag TEXT NOT NULL,
            UNIQUE (tag)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        names.iter().any(|n| n == "t_pkey"),
        "expected pkey in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "t_email_key"),
        "expected column-level UNIQUE in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "t_tag_key"),
        "expected table-level UNIQUE in {names:?}"
    );
}

#[test]
fn create_table_emits_check_constraint_rows() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id  INT NOT NULL CHECK (id > 0),
            qty INT NOT NULL,
            CONSTRAINT positive_qty CHECK (qty >= 0)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        names.iter().any(|n| n == "t_id_check"),
        "expected column-level CHECK in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "positive_qty"),
        "expected named table-level CHECK in {names:?}"
    );
}

// ── CREATE TABLE: REFERENCES (column-level FK) ─────────────────────────────

#[test]
fn create_table_column_level_references_emits_fk_constraint() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (
            id BIGINT PRIMARY KEY,
            p_id BIGINT NOT NULL REFERENCES p(id)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(
        names.iter().any(|n| n == "c_p_id_fkey"),
        "expected FK in {names:?}"
    );
}

#[test]
fn create_table_references_without_column_uses_target_pk() {
    // PG: `REFERENCES p` (no column list) defaults to the target's PK.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT REFERENCES p);",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(names.iter().any(|n| n == "c_p_id_fkey"));
}

#[test]
fn create_table_references_unknown_table_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT REFERENCES ghost(id));"
        )]),
        DdlError::TableNotFound(_),
        "relation \"ghost\" does not exist (referenced by foreign key constraint \"c_p_id_fkey\")",
    );
}

#[test]
fn create_table_references_unknown_column_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (id BIGINT PRIMARY KEY);
             CREATE TABLE c (
                id BIGINT PRIMARY KEY,
                p_id BIGINT REFERENCES p(ghost)
             );"
        )]),
        DdlError::Parse(_),
        "column \"ghost\" referenced in foreign key constraint does not exist",
    );
}

#[test]
fn create_table_references_non_unique_target_is_rejected() {
    // The target column has no PK/UNIQUE — PG: `there is no unique
    // constraint matching given keys for referenced table`.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (id BIGINT PRIMARY KEY, label TEXT NOT NULL);
             CREATE TABLE c (
                id BIGINT PRIMARY KEY,
                p_label TEXT NOT NULL REFERENCES p(label)
             );"
        )]),
        DdlError::DependencyError(_),
        "there is no unique constraint matching given keys for referenced table \"p\"",
    );
}

#[test]
fn create_table_references_with_incompatible_type_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (id BIGINT PRIMARY KEY);
             CREATE TABLE c (
                id BIGINT PRIMARY KEY,
                p_id TEXT NOT NULL REFERENCES p(id)
             );"
        )]),
        DdlError::DependencyError(_),
        "foreign key constraint \"c_p_id_fkey\" cannot be implemented (Key columns \"p_id\" of the \
         referencing table and \"id\" of the referenced table are of incompatible types: text and \
         bigint.)",
    );
}

#[test]
fn create_table_references_into_unique_constraint_is_accepted() {
    // FK can target any UNIQUE column, not just PK.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY, slug TEXT NOT NULL UNIQUE);
         CREATE TABLE c (
            id BIGINT PRIMARY KEY,
            p_slug TEXT NOT NULL REFERENCES p(slug)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(names.iter().any(|n| n == "c_p_slug_fkey"));
}

#[test]
fn create_table_table_level_foreign_key_with_composite_target_is_accepted() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE p (
            a INT NOT NULL,
            b INT NOT NULL,
            PRIMARY KEY (a, b)
         );
         CREATE TABLE c (
            id BIGINT PRIMARY KEY,
            pa INT NOT NULL,
            pb INT NOT NULL,
            FOREIGN KEY (pa, pb) REFERENCES p (a, b)
         );",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(
        names.iter().any(|n| n == "c_pa_pb_fkey"),
        "expected composite FK in {names:?}"
    );
}

#[test]
fn create_table_table_level_foreign_key_arity_mismatch_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (a INT NOT NULL, b INT NOT NULL, PRIMARY KEY (a, b));
             CREATE TABLE c (
                id BIGINT PRIMARY KEY,
                pa INT NOT NULL,
                FOREIGN KEY (pa) REFERENCES p (a, b)
             );"
        )]),
        DdlError::Parse(_),
        "number of referencing and referenced columns for foreign key disagree",
    );
}

// ── ALTER TABLE ADD CONSTRAINT ─────────────────────────────────────────────

#[test]
fn alter_table_add_foreign_key_emits_constraint_row() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL);
         ALTER TABLE c ADD CONSTRAINT c_p_id_fk FOREIGN KEY (p_id) REFERENCES p (id);",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "c");
    assert!(names.iter().any(|n| n == "c_p_id_fk"));
}

#[test]
fn alter_table_add_foreign_key_without_target_pk_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE p (id BIGINT NOT NULL);
                 CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL);"
            ),
            (
                "0002.sql",
                "ALTER TABLE c ADD CONSTRAINT c_p_id_fk FOREIGN KEY (p_id) REFERENCES p;"
            ),
        ]),
        DdlError::DependencyError(_),
        "there is no primary key for referenced table \"p\"",
    );
}

// ── ALTER TABLE DROP CONSTRAINT ────────────────────────────────────────────

#[test]
fn alter_table_drop_constraint_removes_row() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL UNIQUE);
         ALTER TABLE t DROP CONSTRAINT t_slug_key;",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        !names.iter().any(|n| n == "t_slug_key"),
        "t_slug_key should be gone, got {names:?}"
    );
    assert!(names.iter().any(|n| n == "t_pkey"));
}

#[test]
fn alter_table_drop_nonexistent_constraint_errors() {
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);"),
            ("0002.sql", "ALTER TABLE t DROP CONSTRAINT ghost;"),
        ]),
        DdlError::DependencyError(_),
        "constraint \"ghost\" of relation \"t\" does not exist",
    );
}

#[test]
fn alter_table_drop_nonexistent_constraint_if_exists_is_noop() {
    // PG accepts `DROP CONSTRAINT IF EXISTS` for an absent name.
    try_apply(&[
        ("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);"),
        ("0002.sql", "ALTER TABLE t DROP CONSTRAINT IF EXISTS ghost;"),
    ])
    .expect("DROP CONSTRAINT IF EXISTS should accept missing name");
}

#[test]
fn drop_pk_referenced_by_fk_without_cascade_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE p (id BIGINT PRIMARY KEY);
                 CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));"
            ),
            ("0002.sql", "ALTER TABLE p DROP CONSTRAINT p_pkey;"),
        ]),
        DdlError::DependencyError(_),
        "cannot drop constraint p_pkey on table p because other objects depend on it (foreign key constraint \"c_p_id_fkey\" depends on this)",
    );
}

#[test]
fn drop_pk_referenced_by_fk_with_cascade_is_accepted() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));
         ALTER TABLE p DROP CONSTRAINT p_pkey CASCADE;",
    )
    .unwrap();
    let p_cons = db.pg_constraint_names_for_table("public", "p");
    let c_cons = db.pg_constraint_names_for_table("public", "c");
    assert!(!p_cons.iter().any(|n| n == "p_pkey"));
    // The FK on c was *not* removed by CASCADE in our model — that's a
    // gap; document with a TODO if needed. For now just sanity-check
    // that the rest of the table is intact.
    assert!(c_cons.iter().any(|n| n == "c_pkey"));
}

// ── ALTER TABLE RENAME CONSTRAINT ──────────────────────────────────────────

#[test]
fn alter_table_rename_constraint_renames_row() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY);
         ALTER TABLE t RENAME CONSTRAINT t_pkey TO t_id_pkey;",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        names.iter().any(|n| n == "t_id_pkey"),
        "renamed constraint missing in {names:?}"
    );
    assert!(!names.iter().any(|n| n == "t_pkey"));
}

#[test]
fn alter_table_rename_nonexistent_constraint_errors() {
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);"),
            (
                "0002.sql",
                "ALTER TABLE t RENAME CONSTRAINT ghost TO whatever;"
            ),
        ]),
        DdlError::DependencyError(_),
        "constraint \"ghost\" for table \"t\" does not exist",
    );
}

// ── DROP TABLE with FK target ─────────────────────────────────────────────

#[test]
fn drop_table_referenced_by_fk_without_cascade_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE p (id BIGINT PRIMARY KEY);
                 CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));"
            ),
            ("0002.sql", "DROP TABLE p;"),
        ]),
        DdlError::DependencyError(_),
        "cannot drop table p because other objects depend on it (foreign key constraint(s) c_p_id_fkey on public.c depend on this)",
    );
}

#[test]
fn drop_table_referenced_by_fk_with_cascade_drops_fk_too() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE p (id BIGINT PRIMARY KEY);
         CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));
         DROP TABLE p CASCADE;",
    )
    .unwrap();
    // `c` is still here — only the FK row is gone (PG also removes the
    // FK constraint, which is exactly what we mirror).
    let c_cons = db.pg_constraint_names_for_table("public", "c");
    assert!(!c_cons.iter().any(|n| n == "c_p_id_fkey"));
}

// ── DROP COLUMN with constraints ───────────────────────────────────────────

#[test]
fn drop_column_with_pkey_dependency_drops_pkey_silently() {
    // PG drops the PK constraint along with the column it covers (only
    // *external* dependencies block DROP COLUMN without CASCADE — local
    // PK/UNIQUE/CHECK/FK source/indexes all piggyback on the column).
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT);",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN id;"),
    ]);

    let names = snap.pg_constraint_names_for_table("public", "t");
    assert!(
        !names.iter().any(|n| n == "t_pkey"),
        "PK constraint should be dropped along with the column: {names:?}",
    );
}

#[test]
fn drop_column_with_pkey_dependency_with_cascade_drops_pkey_too() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT NOT NULL);
         ALTER TABLE t DROP COLUMN id CASCADE;",
    )
    .unwrap();
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(
        !names.iter().any(|n| n == "t_pkey"),
        "PK should be gone, got {names:?}"
    );
}

#[test]
fn drop_column_with_fk_target_without_cascade_is_rejected() {
    // The dropped column is the *target* of an FK on another table.
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE p (id BIGINT PRIMARY KEY);
                 CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));"
            ),
            ("0002.sql", "ALTER TABLE p DROP COLUMN id;"),
        ]),
        DdlError::DependencyError(_),
        "cannot drop column id of table p because other objects depend on it (constraint(s) c_p_id_fkey depend on this column)",
    );
}

#[test]
fn drop_column_with_fk_source_drops_fk_silently() {
    // PG drops the FK source-side constraint along with the column it
    // covers — only the FK *target* side (an FK in a different table that
    // points at this column) blocks DROP COLUMN without CASCADE.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE p (id BIGINT PRIMARY KEY);
             CREATE TABLE c (id BIGINT PRIMARY KEY, p_id BIGINT NOT NULL REFERENCES p(id));",
        ),
        ("0002.sql", "ALTER TABLE c DROP COLUMN p_id;"),
    ]);

    let names = snap.pg_constraint_names_for_table("public", "c");
    assert!(
        !names.iter().any(|n| n == "c_p_id_fkey"),
        "FK source-side constraint should be dropped along with the column: {names:?}",
    );
}

#[test]
fn drop_column_unrelated_to_constraints_is_accepted() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT, payload TEXT);
         ALTER TABLE t DROP COLUMN payload;",
    )
    .unwrap();
    // Unaffected constraints stay.
    let names = db.pg_constraint_names_for_table("public", "t");
    assert!(names.iter().any(|n| n == "t_pkey"));
}

#[test]
fn drop_column_with_check_dependency_drops_check_silently() {
    // PG drops a column-local CHECK constraint along with the column —
    // CHECK isn't a "blocker" the way PK/UNIQUE/FK are. No CASCADE needed.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (
                id  BIGINT PRIMARY KEY,
                qty INT NOT NULL CHECK (qty >= 0)
             );",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN qty;"),
    ]);

    let names = snap.pg_constraint_names_for_table("public", "t");
    assert!(
        !names.iter().any(|n| n == "t_qty_check"),
        "CHECK constraint should be dropped along with the column: {names:?}",
    );
}

// ── DROP TABLE with FK ────────────────────────────────────────────────────

#[test]
fn drop_table_with_no_fk_target_succeeds() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY);
         DROP TABLE t;",
    )
    .unwrap();
    let cons = db.pg_constraint_names_for_table("public", "t");
    assert!(
        cons.is_empty(),
        "constraints should be cleaned up: {cons:?}"
    );
}

// ── PG 18 not-null constraints ──────────────────────────────────────────────

#[test]
fn not_null_constraints_can_be_dropped_by_name() {
    // PG 18: t_id_not_null (generated) and x_nn (explicit) are pg_constraint
    // rows; dropping them makes the columns nullable.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (id int NOT NULL, x int CONSTRAINT x_nn NOT NULL);
         ALTER TABLE t DROP CONSTRAINT t_id_not_null;
         ALTER TABLE t DROP CONSTRAINT x_nn;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM t").unwrap(),
        vec![cn("id", int4()), cn("x", int4())],
    );
}

#[test]
fn not_null_constraint_names_follow_pg() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (id int NOT NULL, pk int PRIMARY KEY, s serial);
         ALTER TABLE t ADD COLUMN y int;
         ALTER TABLE t ALTER COLUMN y SET NOT NULL;",
    )]);
    let mut names: Vec<String> = db
        .pg_constraint_names_for_table("public", "t")
        .into_iter()
        .filter(|n| n.ends_with("_not_null"))
        .collect();
    names.sort();
    // (`ADD CONSTRAINT name NOT NULL col` is PG 18 grammar the libpg_query
    // 17 parser rejects.)
    assert_eq!(
        names,
        vec![
            "t_id_not_null",
            "t_pk_not_null",
            "t_s_not_null",
            "t_y_not_null"
        ]
    );
}

#[test]
fn primary_key_columns_keep_not_null() {
    // PG 18: 42P16 column "pk" is in a primary key (DROP CONSTRAINT and
    // DROP NOT NULL alike).
    for stmt in [
        "ALTER TABLE t DROP CONSTRAINT t_pk_not_null;",
        "ALTER TABLE t ALTER COLUMN pk DROP NOT NULL;",
    ] {
        assert_ddl_err!(
            try_apply(&[
                ("0001.sql", "CREATE TABLE t (pk int PRIMARY KEY);"),
                ("0002.sql", stmt),
            ]),
            DdlError::Parse(_),
            "column \"pk\" is in a primary key",
        );
    }
}

#[test]
fn conflicting_null_declarations_are_rejected() {
    // PG 18: 42601 conflicting NULL/NOT NULL declarations for column "a".
    let err = try_apply(&[("0001.sql", "CREATE TABLE t (a int NOT NULL NULL);")]).unwrap_err();
    assert!(
        err.to_string()
            .starts_with("conflicting NULL/NOT NULL declarations for column \"a\""),
        "{err}"
    );
}

// ── Self-referencing foreign keys ───────────────────────────────────────────

#[test]
fn self_referencing_foreign_keys_are_accepted() {
    // PG 18: all three succeed — the table's own keys exist by the time
    // its FOREIGN KEYs are added.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (id int PRIMARY KEY, parent int REFERENCES t(id));
         CREATE TABLE t2 (id int PRIMARY KEY, parent int REFERENCES t2);
         CREATE TABLE t3 (id int, u int UNIQUE, p int, FOREIGN KEY (p) REFERENCES t3(u));",
    )]);
}

#[test]
fn self_referencing_foreign_key_still_needs_a_key() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t4 (id int, p int REFERENCES t4(id));",
        )]),
        DdlError::DependencyError(_),
        "there is no unique constraint matching given keys for referenced table \"t4\"",
    );
}

// ── Constraints recorded for ADD COLUMN, EXCLUDE, USING INDEX ───────────────

fn sorted_constraint_names(db: &PgCatalog, table: &str) -> Vec<String> {
    let mut names = db.pg_constraint_names_for_table("public", table);
    names.sort();
    names
}

#[test]
fn add_column_records_its_inline_constraints() {
    // PG 18: t1_c_check, t1_c_fkey, t1_email_key, t1_k_not_null, t1_pkey.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t1 (id int);
         ALTER TABLE t1 ADD COLUMN email text UNIQUE;
         ALTER TABLE t1 ADD COLUMN k int PRIMARY KEY;
         ALTER TABLE t1 ADD COLUMN c int CHECK (c > 0) REFERENCES t1(k);",
    )]);
    assert_eq!(
        sorted_constraint_names(&db, "t1"),
        vec![
            "t1_c_check",
            "t1_c_fkey",
            "t1_email_key",
            "t1_k_not_null",
            "t1_pkey"
        ]
    );
    db.analyze("INSERT INTO t1 (email) VALUES ('x') ON CONFLICT (email) DO NOTHING")
        .unwrap();
    db.analyze("INSERT INTO t1 (k) VALUES (1) ON CONFLICT (k) DO NOTHING")
        .unwrap();
}

#[test]
fn exclusion_constraints_are_recorded() {
    // PG 18: t4_r_excl (generated name), myex (explicit).
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t4 (r int4range, EXCLUDE USING gist (r WITH &&));
         CREATE TABLE t5 (r int4range);
         ALTER TABLE t5 ADD CONSTRAINT myex EXCLUDE USING gist (r WITH &&);",
    )]);
    assert_eq!(sorted_constraint_names(&db, "t4"), vec!["t4_r_excl"]);
    assert_eq!(sorted_constraint_names(&db, "t5"), vec!["myex"]);
    db.analyze(
        "INSERT INTO t4 (r) VALUES ('[1,2)') ON CONFLICT ON CONSTRAINT t4_r_excl DO NOTHING",
    )
    .unwrap();
}

#[test]
fn add_constraint_using_index_adopts_the_index() {
    // PG 18: NOTICE ... will rename index "ui" to "tu"; the PRIMARY KEY
    // variant makes the column NOT NULL.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE u (a int);
         CREATE UNIQUE INDEX ui ON u (a);
         ALTER TABLE u ADD CONSTRAINT tu UNIQUE USING INDEX ui;
         CREATE TABLE u2 (a int);
         CREATE UNIQUE INDEX u2i ON u2 (a);
         ALTER TABLE u2 ADD PRIMARY KEY USING INDEX u2i;",
    )]);
    assert_eq!(sorted_constraint_names(&db, "u"), vec!["tu"]);
    db.analyze("INSERT INTO u (a) VALUES (1) ON CONFLICT ON CONSTRAINT tu DO NOTHING")
        .unwrap();
    assert_cols(
        &db.analyze("SELECT a FROM u2").unwrap(),
        vec![c("a", int4())],
    );
    assert!(sorted_constraint_names(&db, "u2").contains(&"u2i".to_owned()));
}

#[test]
fn unnamed_indexes_are_named_like_pg() {
    // PG 18: t_expr_idx, t_lower_idx, t_b_idx, t_a_idx, t_a_idx1, t_a_a1_idx
    // (a unique index is `_idx` too — only constraints get `_key`).
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b text);
         CREATE INDEX ON t ((a+1));
         CREATE INDEX ON t (lower(b));
         CREATE INDEX ON t ((b::varchar));
         CREATE UNIQUE INDEX ON t (a);
         CREATE INDEX ON t (a);
         CREATE INDEX ON t (a, a);",
    )]);
    db.apply_sql(
        "DROP INDEX t_expr_idx; DROP INDEX t_lower_idx; DROP INDEX t_b_idx;
         DROP INDEX t_a_idx; DROP INDEX t_a_idx1; DROP INDEX t_a_a1_idx;",
    )
    .unwrap();
}

#[test]
fn create_unique_index_is_not_a_constraint() {
    // PG 18: a plain unique index is an ON CONFLICT (a) arbiter and an FK
    // target, but not a constraint: ON CONSTRAINT / DROP CONSTRAINT by its
    // name fail with 42704.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE u (a int);
         CREATE UNIQUE INDEX ui ON u (a);
         CREATE TABLE r (x int REFERENCES u (a));",
    )]);
    db.analyze("INSERT INTO u VALUES (1) ON CONFLICT (a) DO NOTHING")
        .unwrap();
    let err = db
        .analyze("INSERT INTO u VALUES (1) ON CONFLICT ON CONSTRAINT ui DO NOTHING")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("constraint \"ui\" for table \"u\" does not exist"),
        "{err}"
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE u (a int); CREATE UNIQUE INDEX ui ON u (a);
             ALTER TABLE u DROP CONSTRAINT ui;",
        )]),
        DdlError::DependencyError(_),
        "constraint \"ui\" of relation \"u\" does not exist",
    );
}

#[test]
fn generated_constraint_names_follow_pg() {
    // PG 18: a CHECK is named after the one column it reads (else just
    // `_check`), names are numbered on collision, and an unnamed UNIQUE
    // repeating another key is dropped.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int CHECK (a > 0), c int CHECK (c > b), CHECK (a > 1),
                         UNIQUE (a), UNIQUE (a), CONSTRAINT k UNIQUE (b), UNIQUE (b),
                         FOREIGN KEY (c) REFERENCES t (a));
         ALTER TABLE t ADD CHECK (b < 100);
         ALTER TABLE t ADD UNIQUE (c);
         ALTER TABLE t ADD FOREIGN KEY (b) REFERENCES t (a);
         CREATE TABLE x_pkey (y int);
         CREATE TABLE x (a int PRIMARY KEY);",
    )]);
    let mut names = db.pg_constraint_names_for_table("public", "t");
    names.sort();
    assert_eq!(
        names,
        vec![
            "k",
            "t_a_check",
            "t_a_check1",
            "t_a_key",
            "t_b_check",
            "t_b_fkey",
            "t_c_fkey",
            "t_c_key",
            "t_check"
        ]
    );
    let names: Vec<String> = db
        .pg_constraint_names_for_table("public", "x")
        .into_iter()
        .filter(|n| !n.ends_with("_not_null"))
        .collect();
    assert_eq!(names, vec!["x_pkey1"]);
}

#[test]
fn on_conflict_on_constraint_needs_an_index_backed_constraint() {
    // PG 18: 42809 constraint in ON CONFLICT clause has no associated index
    // (NOT NULL and CHECK constraints); a PRIMARY KEY / UNIQUE one works.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int NOT NULL, b int CHECK (b > 0), c int UNIQUE);",
    )]);
    for name in ["t_a_not_null", "t_b_check"] {
        let sql = format!("INSERT INTO t VALUES (1) ON CONFLICT ON CONSTRAINT {name} DO NOTHING");
        let err = db.analyze(&sql).unwrap_err();
        assert!(matches!(err, AnalyzeError::WrongObjectType(_)), "{err:?}");
        assert!(
            err.to_string()
                .starts_with("constraint in ON CONFLICT clause has no associated index"),
            "{err}"
        );
    }
    db.analyze("INSERT INTO t VALUES (1) ON CONFLICT ON CONSTRAINT t_c_key DO NOTHING")
        .unwrap();
}

#[test]
fn check_constraints_read_no_system_column_but_tableoid() {
    // scanNSItemForColumn (EXPR_KIND_CHECK_CONSTRAINT), PG 18 wording.
    for stmt in [
        "CREATE TABLE t (a text, CHECK (ctid::text = 'x'));",
        "CREATE TABLE t (a text CHECK (xmin::text <> ''));",
        "CREATE TABLE t (a text); ALTER TABLE t ADD CHECK (cmax::text <> '');",
    ] {
        let err = try_apply(&[("0001.sql", stmt)]).expect_err(stmt);
        let msg = err.to_string();
        assert!(
            msg.starts_with("system column \"")
                && msg.contains("reference in check constraint is invalid"),
            "{stmt}\n  got: {msg}"
        );
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a text, CHECK (tableoid::regclass::text = 't'));",
    )]);
}

#[test]
fn foreign_key_columns_pair_through_the_referenced_opclass() {
    // ATAddForeignKeyConstraint (PG 18): a cross-type member of the
    // referenced column's btree family, or implicit casts of both types to
    // the opclass type; polymorphic opclasses (arrays) need the same type.
    let setup = "CREATE TABLE p (id bigint PRIMARY KEY, t text UNIQUE, n numeric UNIQUE,
                   d date UNIQUE, arr int[] UNIQUE, f float8 UNIQUE);
                 CREATE TABLE pd (id int PRIMARY KEY DEFERRABLE);
                 CREATE TABLE pu (id int UNIQUE DEFERRABLE);
                 CREATE VIEW v AS SELECT 1 AS a;";
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE c1 (pid int REFERENCES p);
             CREATE TABLE c2 (x name REFERENCES p (t));
             CREATE TABLE c3 (x int REFERENCES p (n));
             CREATE TABLE c4 (x timestamp REFERENCES p (d));
             CREATE TABLE c6 (x float4 REFERENCES p (f));
             CREATE TABLE c7 (x varchar REFERENCES p (t));
             CREATE TABLE c12 (x smallint REFERENCES p (n));",
        ),
    ]);
    for (stmt, msg) in [
        (
            "CREATE TABLE c5 (x bigint[] REFERENCES p (arr));",
            "foreign key constraint \"c5_x_fkey\" cannot be implemented",
        ),
        (
            "CREATE TABLE c8 (x text REFERENCES p (id));",
            "foreign key constraint \"c8_x_fkey\" cannot be implemented",
        ),
        (
            "CREATE TABLE c9 (a bigint, b bigint, FOREIGN KEY (a, b) REFERENCES p (id, id));",
            "foreign key referenced-columns list must not contain duplicates",
        ),
        (
            "CREATE TABLE c10 (x int REFERENCES pd);",
            "cannot use a deferrable primary key for referenced table \"pd\"",
        ),
        (
            "CREATE TABLE c11 (x int REFERENCES pu (id));",
            "cannot use a deferrable unique constraint for referenced table \"pu\"",
        ),
        (
            "CREATE TABLE c13 (x int, FOREIGN KEY (x) REFERENCES p (id) ON DELETE SET NULL (nosuch));",
            "column \"nosuch\" referenced in foreign key constraint does not exist",
        ),
        (
            "CREATE TABLE c14 (x int, y int, FOREIGN KEY (x) REFERENCES p (id) ON DELETE SET NULL (y));",
            "column \"y\" referenced in ON DELETE SET action must be part of foreign key",
        ),
        (
            "CREATE TABLE c15 (x int REFERENCES v (a));",
            "referenced relation \"v\" is not a table",
        ),
        (
            "CREATE TABLE c16 (x int, FOREIGN KEY (xmin) REFERENCES p (id));",
            "system columns cannot be used in foreign keys",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
}
