//! CREATE / DROP INDEX.
//!
//! Indexes don't change query result types — they're invisible to the
//! analyzer's type/nullability inference. We still parse the statement so
//! that expression indexes can be validated for volatility and so partial
//! unique indexes don't silently pose as ON CONFLICT targets.

use crate::common::*;

/// Filter pg_index rows down to those targeting user-defined relations
/// (everything in the seed lives in `pg_catalog` / `pg_toast` /
/// `information_schema`, so anything in `public` is from the test).
fn user_indexes(db: &PgCatalog) -> Vec<&PgIndex> {
    let public_oid = db.namespace_oid("public").unwrap();
    db.pg_index_values()
        .filter(|i| {
            db.pg_class()
                .get(&i.indrelid)
                .is_some_and(|c| c.relnamespace == public_oid)
        })
        .collect()
}

// ── VOLATILE function rejection in index expressions ────────────────────────

#[test]
fn volatile_function_in_index_expression_should_error() {
    // PG: `functions in index expression must be marked IMMUTABLE`.
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
            (
                "0002.sql",
                "CREATE INDEX idx_random ON t ((random() * id));"
            ),
        ]),
        DdlError::UnsupportedDdl(_),
        "functions in index expression must be marked IMMUTABLE (function \"random\" is not)",
    );
}

#[test]
fn nextval_in_index_expression_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE t (id INT NOT NULL); CREATE SEQUENCE s;"
            ),
            (
                "0002.sql",
                "CREATE INDEX idx_seq ON t ((nextval('s')::int));"
            ),
        ]),
        DdlError::UnsupportedDdl(_),
        "functions in index expression must be marked IMMUTABLE (function \"nextval\" is not)",
    );
}

#[test]
fn plain_column_index_is_accepted() {
    // Plain column indexes have no expression — the volatility walker
    // must not over-reject them.
    try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        ("0002.sql", "CREATE INDEX idx_name ON t (name);"),
    ])
    .expect("plain column index must apply cleanly");
}

#[test]
fn immutable_function_in_index_expression_is_accepted() {
    try_apply(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);",
        ),
        (
            "0002.sql",
            "CREATE INDEX idx_lower_name ON t ((lower(name)));",
        ),
    ])
    .expect("IMMUTABLE function in index must apply cleanly");
}

// ── partial unique indexes don't cover ON CONFLICT ──────────────────────────
//
// PG only treats a unique index as a valid ON CONFLICT target when it has
// no predicate (or the predicate covers every row). A partial unique index
// `WHERE deleted_at IS NULL` does NOT satisfy `ON CONFLICT (slug)` for the
// generic insert. CREATE INDEX skips emitting `pg_constraint` for these,
// so the validator correctly fails to find a match.

#[test]
fn on_conflict_against_partial_unique_index_should_error() {
    // PG rejects ON CONFLICT on a partial-index column at planning time
    // (`there is no unique or exclusion constraint matching the ON CONFLICT
    // specification`). PG sanity's wire-level `prepare` skips planning, so the
    // sanity mirror can't see this — opt out and rely on the analyzer.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL, deleted_at TIMESTAMPTZ);
         CREATE UNIQUE INDEX t_slug_live ON t (slug) WHERE deleted_at IS NULL;",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
             ON CONFLICT (slug) DO NOTHING",
        ),
        AnalyzeError::InvalidColumnReference(_),
        "there is no unique or exclusion constraint matching the ON CONFLICT specification on table \"t\"",
    );
}

#[test]
fn on_conflict_against_full_unique_index_is_accepted() {
    // A non-partial UNIQUE INDEX makes the column a valid ON CONFLICT
    // target — same shape PG uses to back primary-key/unique constraints.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL);
         CREATE UNIQUE INDEX t_slug_uniq ON t (slug);",
    )
    .unwrap();
    db.analyze(
        "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
         ON CONFLICT (slug) DO NOTHING",
    )
    .unwrap();
}

#[test]
fn unique_index_does_not_match_against_distinct_columns() {
    // The unique covers `(a, b)`, not `(a)` alone — same opt-out reason as
    // above (planner-only check, invisible to PG sanity's `prepare`).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (a INT NOT NULL, b INT NOT NULL);
         CREATE UNIQUE INDEX t_ab ON t (a, b);",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO t (a, b) VALUES ($p1, $p2) \
             ON CONFLICT (a) DO NOTHING"
        ),
        AnalyzeError::InvalidColumnReference(_),
        "there is no unique or exclusion constraint matching the ON CONFLICT specification on table \"t\"",
    );
}

#[test]
fn expression_unique_index_does_not_match_column_on_conflict() {
    // PG: ON CONFLICT (lower(slug)) needs an expression-based unique
    // index. We don't model that, so the test just confirms a column
    // ON CONFLICT against a func-only unique index isn't matched.
    //
    // Real PG rejects this at planning time, but `pglite-socket`'s
    // wire-level `prepare` skips planning, so the sanity check can't see
    // it. Disable the mirror — our analyzer is still the load-bearing
    // check at compile time.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL);
         CREATE UNIQUE INDEX t_slug_lower ON t ((lower(slug)));",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
             ON CONFLICT (slug) DO NOTHING"
        ),
        AnalyzeError::InvalidColumnReference(_),
        "there is no unique or exclusion constraint matching the ON CONFLICT specification on table \"t\"",
    );
}

// ── pg_index modeling ───────────────────────────────────────────────────────
//
// CREATE INDEX writes pg_class (relkind = 'i') + pg_index. PK/UNIQUE
// inline constraints write the same backing rows so the constraint and
// its supporting index share a name and round-trip through DROP/RENAME
// the way PG does it.

#[test]
fn create_index_emits_pg_class_and_pg_index() {
    let db = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
         CREATE INDEX t_name_idx ON t (name);",
    )]);

    let idx_class = db.resolve_table(None, "t_name_idx").unwrap();
    assert!(matches!(idx_class.relkind, RelKind::Index));
    assert!(idx_class.reltype.is_none());

    let table_oid = db.resolve_table(None, "t").unwrap().oid;
    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == idx_class.oid)
        .expect("pg_index row missing for created index");
    assert_eq!(idx.indrelid, table_oid);
    assert_eq!(idx.indkey, vec![2]); // name is attnum 2
    assert_eq!(idx.indnatts, 1);
    assert!(!idx.indisunique);
    assert!(!idx.indisprimary);
    assert!(idx.indpred.is_none());
    assert!(idx.indexprs.is_empty());
}

#[test]
fn primary_key_emits_backing_pg_index() {
    let db = build(&[("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);")]);

    let pkey_class = db.resolve_table(None, "t_pkey").unwrap();
    assert!(matches!(pkey_class.relkind, RelKind::Index));

    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == pkey_class.oid)
        .expect("backing pg_index row missing for PRIMARY KEY");
    assert!(idx.indisprimary);
    assert!(idx.indisunique);
    assert_eq!(idx.indkey, vec![1]);
}

#[test]
fn unique_index_with_partial_predicate_records_indpred() {
    let db = build(&[(
        "0001.sql",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL, deleted_at TIMESTAMPTZ);
         CREATE UNIQUE INDEX t_slug_live ON t (slug) WHERE deleted_at IS NULL;",
    )]);

    let idx_class = db.resolve_table(None, "t_slug_live").unwrap();
    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == idx_class.oid)
        .unwrap();
    assert!(idx.indisunique);
    assert!(
        idx.indpred.is_some(),
        "partial unique index should populate indpred"
    );
}

#[test]
fn expression_index_records_indexprs_with_zero_indkey_slots() {
    let db = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, slug TEXT NOT NULL);
         CREATE INDEX t_lower_slug ON t (id, (lower(slug)));",
    )]);

    let idx_class = db.resolve_table(None, "t_lower_slug").unwrap();
    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == idx_class.oid)
        .unwrap();
    // First slot is `id` (attnum 1), second is the expression (0).
    assert_eq!(idx.indkey, vec![1, 0]);
    assert_eq!(idx.indexprs.len(), 1);
}

#[test]
fn create_index_duplicate_name_in_schema_errors() {
    // PG: `relation "t_idx" already exists`. The index shares pg_class with
    // tables/views, so the name must be unique within a schema.
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
                 CREATE INDEX my_idx ON t (id);",
            ),
            ("0002.sql", "CREATE INDEX my_idx ON t (name);"),
        ]),
        DdlError::DuplicateObject(_),
        "relation \"my_idx\" already exists",
    );
}

#[test]
fn create_index_if_not_exists_skips_duplicate() {
    // PG: `IF NOT EXISTS` swallows the duplicate-name error and leaves
    // the existing index in place.
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE INDEX my_idx ON t (id);",
        ),
        ("0002.sql", "CREATE INDEX IF NOT EXISTS my_idx ON t (name);"),
    ]);

    let idx_class = db.resolve_table(None, "my_idx").unwrap();
    let idx = db
        .pg_index_values()
        .find(|i| i.indexrelid == idx_class.oid)
        .unwrap();
    // Still indexes `id` (attnum 1) — the second statement was a no-op.
    assert_eq!(idx.indkey, vec![1]);
}

#[test]
fn drop_index_removes_pg_class_and_pg_index_rows() {
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE INDEX t_name_idx ON t (name);",
        ),
        ("0002.sql", "DROP INDEX t_name_idx;"),
    ]);

    assert!(db.resolve_table(None, "t_name_idx").is_none());
    // Filter to user-relations: pg_catalog ships with its own indexes.
    assert!(
        user_indexes(&db).is_empty(),
        "no user pg_index rows should remain after DROP INDEX"
    );
}

#[test]
fn drop_table_cascades_to_indexes() {
    // Even without CASCADE, indexes belong to their table — DROP TABLE
    // tears them down implicitly. Mirrors PG.
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL);
             CREATE INDEX t_name_idx ON t (name);",
        ),
        ("0002.sql", "DROP TABLE t;"),
    ]);

    assert!(db.resolve_table(None, "t_name_idx").is_none());
    assert!(user_indexes(&db).is_empty());
}

#[test]
fn drop_index_missing_without_if_exists_errors() {
    assert_ddl_err!(
        try_apply(&[("0001.sql", "DROP INDEX no_such_idx;")]),
        DdlError::DependencyError(_),
        "index \"no_such_idx\" does not exist",
    );
}

#[test]
fn drop_index_if_exists_no_error_when_missing() {
    let _ = build(&[("0001.sql", "DROP INDEX IF EXISTS no_such_idx;")]);
}

#[test]
fn drop_index_when_target_is_a_table_errors() {
    // PG: `"t" is not an index`. DROP INDEX must reject when the resolved
    // pg_class row isn't relkind = 'i'.
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
            ("0002.sql", "DROP INDEX t;"),
        ]),
        DdlError::DependencyError(_),
        "\"t\" is not an index",
    );
}

#[test]
fn alter_table_drop_constraint_removes_backing_index() {
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL UNIQUE);",
        ),
        ("0002.sql", "ALTER TABLE t DROP CONSTRAINT t_slug_key;"),
    ]);

    assert!(db.resolve_table(None, "t_slug_key").is_none());
    // Only the pkey index should remain on `t`. Filter by `indrelid` so
    // built-in pg_catalog indexes don't pollute the assertion.
    let table_oid = db.resolve_table(None, "t").unwrap().oid;
    let on_t: Vec<_> = db
        .pg_index_values()
        .filter(|i| i.indrelid == table_oid)
        .collect();
    assert_eq!(on_t.len(), 1);
    assert!(on_t[0].indisprimary);
}

#[test]
fn drop_column_referenced_by_index_drops_index_silently() {
    // PG only blocks DROP COLUMN on *external* dependencies (FKs in other
    // tables, views). Indexes that exist purely on this column piggyback on
    // the column drop — they're removed silently along with the column,
    // CASCADE not required.
    let snap = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, slug TEXT NOT NULL);
             CREATE INDEX t_slug_idx ON t (slug);",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN slug;"),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert!(
        snap.pg_index_values().all(|i| i.indrelid != table.oid),
        "index on dropped column should be removed silently",
    );
}

#[test]
fn drop_column_cascade_removes_dependent_index() {
    let db = build(&[
        (
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, slug TEXT NOT NULL);
             CREATE INDEX t_slug_idx ON t (slug);",
        ),
        ("0002.sql", "ALTER TABLE t DROP COLUMN slug CASCADE;"),
    ]);

    assert!(db.resolve_table(None, "t_slug_idx").is_none());
    assert!(user_indexes(&db).is_empty());
}

// ── Catalog noise must not leak into ON CONFLICT lookups ───────────────────
//
// The seed ships with ~163 pg_index rows (every catalog table has its own
// indexes). ON CONFLICT (cols) matches pg_constraint by relation OID +
// conkey set, so if a pg_catalog index named like a user column were
// reachable, the validator could return a false positive. These tests
// pin the property by exercising column names that show up frequently
// across pg_catalog (`oid`, `name`).

#[test]
fn on_conflict_user_column_not_polluted_by_catalog_indexes() {
    // `oid` is indexed across pg_catalog (`pg_class_oid_index`,
    // `pg_proc_oid_index`, …). A user table with an `oid` column should
    // still need its OWN unique constraint to participate in ON CONFLICT.
    // PG sanity's `prepare` skips planning, so opt out of the sanity mirror.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE my_objs (oid BIGINT NOT NULL, name TEXT NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO my_objs (oid, name) VALUES ($p1, $p2) \
             ON CONFLICT (oid) DO NOTHING"
        ),
        AnalyzeError::InvalidColumnReference(_),
        "there is no unique or exclusion constraint matching the ON CONFLICT specification on table \"my_objs\"",
    );
}

#[test]
fn user_unique_index_still_resolves_on_conflict() {
    // Sanity: with the noisy catalog still loaded, a user-created UNIQUE
    // INDEX on a column whose name collides with a catalog column must
    // still resolve.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE my_objs (oid BIGINT NOT NULL, name TEXT NOT NULL);
         CREATE UNIQUE INDEX my_objs_oid_uniq ON my_objs (oid);",
    )
    .unwrap();
    db.analyze(
        "INSERT INTO my_objs (oid, name) VALUES ($p1, $p2) \
         ON CONFLICT (oid) DO NOTHING",
    )
    .unwrap();
}

#[test]
fn alter_table_rename_constraint_renames_backing_index() {
    let db = build(&[
        ("0001.sql", "CREATE TABLE t (id BIGINT PRIMARY KEY);"),
        (
            "0002.sql",
            "ALTER TABLE t RENAME CONSTRAINT t_pkey TO t_primary;",
        ),
    ]);

    assert!(db.resolve_table(None, "t_pkey").is_none());
    let renamed = db.resolve_table(None, "t_primary").unwrap();
    assert!(matches!(renamed.relkind, RelKind::Index));
}

// ── CREATE INDEX targets (DefineIndex / ComputeIndexAttrs) ──────────────────

#[test]
fn create_index_target_errors() {
    for (sql, msg) in [
        (
            "CREATE INDEX ON nosuch (a);",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE INDEX IF NOT EXISTS ix ON nosuch (a);",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE TABLE t (a int); CREATE INDEX ON t (nosuch);",
            "column \"nosuch\" does not exist",
        ),
        (
            "CREATE TABLE t (a int); CREATE VIEW v AS SELECT a FROM t; CREATE INDEX ON v (a);",
            "cannot create index on relation \"v\"",
        ),
        (
            "CREATE SEQUENCE s; CREATE INDEX ON s (last_value);",
            "cannot create index on relation \"s\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int);
         CREATE MATERIALIZED VIEW mv AS SELECT a FROM t;
         CREATE INDEX ON mv (a);",
    )]);
}

#[test]
fn stable_functions_are_not_immutable_in_indexes_or_generated_columns() {
    // PG 18: CheckMutability rejects STABLE callees too; a STABLE SQL
    // function that inlines to an immutable expression is fine.
    for (sql, msg) in [
        (
            "CREATE TABLE t (a int); CREATE INDEX ON t ((now()));",
            "functions in index expression must be marked IMMUTABLE",
        ),
        (
            "CREATE TABLE t (ts timestamptz); CREATE INDEX ON t ((to_char(ts, 'YYYY')));",
            "functions in index expression must be marked IMMUTABLE",
        ),
        (
            "CREATE TABLE g (a timestamptz GENERATED ALWAYS AS (now()) STORED);",
            "generation expression is not immutable",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, d text);
         CREATE INDEX ON t (lower(d));
         CREATE INDEX ON t (abs(a));
         CREATE FUNCTION s1(int) RETURNS int STABLE LANGUAGE sql AS 'select $1';
         CREATE INDEX ON t (s1(a));",
    )]);
}

#[test]
fn casts_and_operators_count_for_index_and_generated_mutability() {
    // PG 18: timestamptz → date and timestamptz + interval run STABLE
    // functions; date + int, int → text and AT TIME ZONE do not.
    for (sql, msg) in [
        (
            "CREATE TABLE t (ts timestamptz); CREATE INDEX ON t ((ts::date));",
            "functions in index expression must be marked IMMUTABLE",
        ),
        (
            "CREATE TABLE t (ts timestamptz); CREATE INDEX ON t ((ts + interval '1 day'));",
            "functions in index expression must be marked IMMUTABLE",
        ),
        (
            "CREATE TABLE g (ts timestamptz, x date GENERATED ALWAYS AS (ts::date) STORED);",
            "generation expression is not immutable",
        ),
        (
            "CREATE TABLE g (ts timestamptz);
             ALTER TABLE g ADD COLUMN x date GENERATED ALWAYS AS (ts::date) STORED;",
            "generation expression is not immutable",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (ts timestamptz, d date, a int);
         CREATE INDEX ON t ((d + 1));
         CREATE INDEX ON t ((a::text));
         CREATE INDEX ON t ((ts AT TIME ZONE 'UTC'));",
    )]);
}

#[test]
fn implicit_argument_coercions_count_for_index_mutability() {
    // PG 18: the timestamp → timestamptz coercion of the argument is STABLE,
    // so the index is rejected although tz_g is IMMUTABLE.
    let err = try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (ts timestamp);
         CREATE FUNCTION tz_g(timestamptz) RETURNS int IMMUTABLE LANGUAGE plpgsql
             AS 'begin return 1; end';
         CREATE INDEX ON t (tz_g(ts));",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("functions in index expression must be marked IMMUTABLE"),
        "{err}"
    );
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int);
         CREATE FUNCTION big(bigint) RETURNS bigint IMMUTABLE LANGUAGE plpgsql
             AS 'begin return $1; end';
         CREATE INDEX ON t (big(a));",
    )]);
}

#[test]
fn alter_index_attach_partition_follows_pg() {
    // ATExecAttachPartitionIdx (PG 18): a partitioned index created ON ONLY
    // a table with partitions is invalid until each partition has a
    // matching index attached; each check reports PG's error.
    let setup = "CREATE TABLE t (a int NOT NULL, b int, c text) PARTITION BY RANGE (a);
                 CREATE TABLE t1 PARTITION OF t FOR VALUES FROM (0) TO (10);
                 CREATE TABLE t2 PARTITION OF t FOR VALUES FROM (10) TO (20);
                 CREATE UNIQUE INDEX t_a_idx ON ONLY t (a);
                 CREATE UNIQUE INDEX t1_a_idx ON t1 (a);
                 CREATE UNIQUE INDEX t1_b_idx ON t1 (b);
                 CREATE UNIQUE INDEX t2_a_idx ON t2 (a);
                 CREATE TABLE u (a int);
                 CREATE UNIQUE INDEX u_a ON u (a);";
    for (stmt, msg) in [
        (
            "ALTER INDEX t_a_idx ATTACH PARTITION t1_b_idx;",
            "cannot attach index \"t1_b_idx\" as a partition of index \"t_a_idx\" (The index \
             definitions do not match.)",
        ),
        (
            "ALTER INDEX t_a_idx ATTACH PARTITION t1_a_idx;
             CREATE UNIQUE INDEX t1_a2 ON t1 (a);
             ALTER INDEX t_a_idx ATTACH PARTITION t1_a2;",
            "cannot attach index \"t1_a2\" as a partition of index \"t_a_idx\" (Another index \
             is already attached for partition \"t1\".)",
        ),
        (
            "ALTER INDEX t1_a_idx ATTACH PARTITION t2_a_idx;",
            "ALTER action ATTACH PARTITION cannot be performed on relation \"t1_a_idx\" (This \
             operation is not supported for indexes.)",
        ),
        (
            "ALTER INDEX t_a_idx ATTACH PARTITION t;",
            "\"t\" is not an index",
        ),
        (
            "ALTER INDEX t_a_idx ATTACH PARTITION nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER INDEX t_a_idx ATTACH PARTITION u_a;",
            "cannot attach index \"u_a\" as a partition of index \"t_a_idx\" (Index \"u_a\" is \
             not an index on any partition of table \"t\".)",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT t_uq UNIQUE USING INDEX t_a_idx;",
            "index \"t_a_idx\" is not valid",
        ),
        (
            "CREATE TABLE r (a int REFERENCES t (a));",
            "there is no unique constraint matching given keys for referenced table \"t\"",
        ),
        (
            "CREATE TABLE t4 (a int NOT NULL, b int, c text) PARTITION BY RANGE (a);
             CREATE UNIQUE INDEX t4_a ON t4 (a);
             ALTER TABLE t ATTACH PARTITION t4 FOR VALUES FROM (20) TO (30);
             DROP INDEX t4_a;",
            "cannot drop index t4_a because index t_a_idx requires it",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    // Attaching an index on every partition validates the parent index —
    // the one AttachPartitionEnsureIndexes attached counts.
    let mut db = build_db(&[("0001.sql", setup)]);
    let conflict = "INSERT INTO t VALUES (1) ON CONFLICT (a) DO NOTHING";
    let no_arbiter =
        "there is no unique or exclusion constraint matching the ON CONFLICT specification";
    assert!(
        db.analyze(conflict)
            .unwrap_err()
            .to_string()
            .starts_with(no_arbiter)
    );
    db.apply_sql(
        "CREATE TABLE t4 (a int NOT NULL, b int, c text) PARTITION BY RANGE (a);
         CREATE UNIQUE INDEX t4_a ON t4 (a);
         ALTER TABLE t ATTACH PARTITION t4 FOR VALUES FROM (20) TO (30);
         ALTER INDEX t_a_idx ATTACH PARTITION t1_a_idx;
         ALTER INDEX t_a_idx ATTACH PARTITION t1_a_idx;",
    )
    .unwrap();
    assert!(
        db.analyze(conflict)
            .unwrap_err()
            .to_string()
            .starts_with(no_arbiter)
    );
    db.apply_sql("ALTER INDEX t_a_idx ATTACH PARTITION t2_a_idx;")
        .unwrap();
    db.analyze(conflict).unwrap();
    db.apply_sql("CREATE TABLE r (a int REFERENCES t (a));")
        .unwrap();
}

#[test]
fn attaching_indexes_of_constraints() {
    // A constraint's partitioned index needs a constraint's index; a plain
    // one takes a constraint index, whose constraint stays local.
    let setup = "CREATE TABLE p (a int, b int) PARTITION BY LIST (a);
                 CREATE TABLE p1 PARTITION OF p FOR VALUES IN (1);
                 CREATE TABLE q (a int, b int) PARTITION BY LIST (a);
                 CREATE TABLE q1 PARTITION OF q FOR VALUES IN (1);";
    for (stmt, msg) in [
        (
            "ALTER TABLE ONLY p ADD CONSTRAINT p_uq UNIQUE (a);
             CREATE UNIQUE INDEX p1_a ON p1 (a);
             ALTER INDEX p_uq ATTACH PARTITION p1_a;",
            "cannot attach index \"p1_a\" as a partition of index \"p_uq\" (The index \"p_uq\" \
             belongs to a constraint in table \"p\" but no constraint exists for index \
             \"p1_a\".)",
        ),
        (
            "ALTER TABLE ONLY q1 ADD CONSTRAINT q1_a UNIQUE (a);
             CREATE INDEX q_b ON ONLY q (b);
             ALTER INDEX q_b ATTACH PARTITION q1_a;",
            "cannot attach index \"q1_a\" as a partition of index \"q_b\" (The index \
             definitions do not match.)",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE UNIQUE INDEX q_a ON ONLY q (a);
             ALTER TABLE ONLY q1 ADD CONSTRAINT q1_a UNIQUE (a);
             ALTER INDEX q_a ATTACH PARTITION q1_a;",
        ),
    ]);
    let con = db
        .constraints_of_table("q1")
        .into_iter()
        .find(|c| c.conname == "q1_a")
        .unwrap();
    assert!(con.conislocal && con.coninhcount == 0);
}

/// Apply `setup` then each statement, expecting PG's error `message` (a
/// prefix of the analyzer's) for every one.
fn assert_rejected(setup: &str, cases: &[(&str, &str)]) {
    for (sql, message) in cases {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", sql)]).expect_err(sql);
        assert!(
            err.to_string().starts_with(message),
            "{sql}\n  expected: {message}\n       got: {err}"
        );
    }
}

/// Apply `setup` then each statement, expecting all to succeed.
fn assert_accepted(setup: &str, statements: &[&str]) {
    for sql in statements {
        if let Err(err) = try_apply(&[("0001.sql", setup), ("0002.sql", sql)]) {
            panic!("{sql}\n  rejected: {err}");
        }
    }
}

#[test]
fn constraint_indexes_resolve_operator_classes() {
    // DefineIndex runs ResolveOpClass for a PRIMARY KEY / UNIQUE / EXCLUDE
    // constraint's index as for CREATE INDEX.
    assert_rejected(
        "CREATE TABLE s (a point, j json);",
        &[
            (
                "CREATE TABLE t (a point UNIQUE);",
                "data type point has no default operator class for access method \"btree\"",
            ),
            (
                "CREATE TABLE t (a json PRIMARY KEY);",
                "data type json has no default operator class for access method \"btree\"",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE USING gist (a WITH =));",
                "data type integer has no default operator class for access method \"gist\"",
            ),
            (
                "ALTER TABLE s ADD UNIQUE (a);",
                "data type point has no default operator class for access method \"btree\"",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE USING btree (a text_ops WITH =));",
                "operator class \"text_ops\" does not accept data type integer",
            ),
        ],
    );
    assert_accepted(
        "",
        &[
            "CREATE TABLE t (a int UNIQUE, b text PRIMARY KEY, \
             c int4range, EXCLUDE USING gist (c WITH &&));",
            "CREATE EXTENSION btree_gist; \
             CREATE TABLE t (a int, r int4range, EXCLUDE USING gist (a WITH =, r WITH &&));",
        ],
    );
}

#[test]
fn constraint_index_options_are_validated() {
    // DefineIndex: index_reloptions over the constraint's WITH (...).
    assert_rejected(
        "CREATE TABLE s (a int);",
        &[
            (
                "CREATE TABLE t (a int, UNIQUE (a) WITH (foo=50));",
                "unrecognized parameter \"foo\"",
            ),
            (
                "CREATE TABLE t (a int PRIMARY KEY WITH (fillfactor=5));",
                "value 5 out of bounds for option \"fillfactor\"",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE USING btree (a WITH =) WITH (foo=1));",
                "unrecognized parameter \"foo\"",
            ),
            (
                "ALTER TABLE s ADD UNIQUE (a) WITH (foo=1);",
                "unrecognized parameter \"foo\"",
            ),
        ],
    );
    assert_accepted(
        "",
        &["CREATE TABLE t (a int PRIMARY KEY WITH (fillfactor=50));"],
    );
}

#[test]
fn exclusion_operators_are_checked() {
    // ComputeIndexAttrs: the operator must exist for the column type, be
    // its own commutator and belong to the operator class's family.
    assert_rejected(
        "",
        &[
            (
                "CREATE TABLE t (r int4range, EXCLUDE USING gist (r WITH <<));",
                "operator <<(anyrange,anyrange) is not commutative",
            ),
            (
                "CREATE TABLE t (r int4range, EXCLUDE USING gist (r WITH <));",
                "operator <(anyrange,anyrange) is not commutative",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE USING btree (a WITH <>));",
                "operator <>(integer,integer) is not a member of operator family \"integer_ops\"",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE USING btree (a WITH ===));",
                "operator does not exist: integer === integer",
            ),
            (
                "CREATE TABLE t (a text, EXCLUDE USING btree (a text_pattern_ops WITH ~=~));",
                "operator does not exist: text ~=~ text",
            ),
        ],
    );
    assert_accepted(
        "",
        &[
            "CREATE TABLE t (a int, EXCLUDE USING btree (a WITH =));",
            "CREATE TABLE t (a int, EXCLUDE USING hash (a WITH =));",
            "CREATE TABLE t (a text, EXCLUDE USING btree (a WITH OPERATOR(pg_catalog.=)));",
            "CREATE EXTENSION btree_gist; CREATE TABLE t (a int, EXCLUDE USING gist (a WITH <>));",
        ],
    );
}

#[test]
fn exclusion_needs_an_access_method_with_gettuple() {
    assert_rejected(
        "",
        &[
            (
                "CREATE TABLE t (a int, EXCLUDE USING brin (a WITH =));",
                "access method \"brin\" does not support exclusion constraints",
            ),
            (
                "CREATE TABLE t (a int[], EXCLUDE USING gin (a WITH &&));",
                "access method \"gin\" does not support exclusion constraints",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE USING nosuch (a WITH =));",
                "access method \"nosuch\" does not exist",
            ),
        ],
    );
}

#[test]
fn exclusion_on_a_partitioned_table_compares_the_partition_key_with_equality() {
    assert_rejected(
        "",
        &[
            (
                "CREATE TABLE t (a int, b int, EXCLUDE USING btree (b WITH =)) \
                 PARTITION BY RANGE (a);",
                "unique constraint on partitioned table must include all partitioning columns",
            ),
            (
                "CREATE TABLE t (a int4range, EXCLUDE USING gist (a WITH &&)) \
                 PARTITION BY RANGE (a);",
                "cannot match partition key to index on column \"a\" using non-equal operator \
                 \"&&\"",
            ),
        ],
    );
    assert_accepted(
        "",
        &[
            "CREATE TABLE t (a int, EXCLUDE USING btree (a WITH =)) PARTITION BY RANGE (a);",
            "CREATE TABLE t (a int, EXCLUDE USING hash (a WITH =)) PARTITION BY RANGE (a);",
            "CREATE TABLE t (a int, EXCLUDE USING btree (a WITH =)) PARTITION BY HASH (a);",
        ],
    );
}

#[test]
fn index_column_collations_are_validated() {
    // ComputeIndexAttrs: the COLLATE clause names an existing collation,
    // of a collatable type.
    let setup = "CREATE TABLE t (a int, b text);";
    assert_rejected(
        setup,
        &[
            (
                "CREATE INDEX ON t (a COLLATE \"C\");",
                "collations are not supported by type integer",
            ),
            (
                "CREATE INDEX ON t ((a + 1) COLLATE \"C\");",
                "collations are not supported by type integer",
            ),
            (
                "CREATE INDEX ON t (b COLLATE nosuch);",
                "collation \"nosuch\" for encoding \"UTF8\" does not exist",
            ),
            (
                "CREATE TABLE x (a text, EXCLUDE USING btree (a COLLATE nosuch WITH =));",
                "collation \"nosuch\" for encoding \"UTF8\" does not exist",
            ),
        ],
    );
    assert_accepted(
        setup,
        &[
            "CREATE INDEX ON t (b COLLATE \"C\");",
            "CREATE INDEX ON t ((b || 'x') COLLATE \"C\");",
        ],
    );
}

#[test]
fn index_max_keys_limits_indexes_partition_keys_and_foreign_keys() {
    let cols = |n: usize| vec!["a"; n].join(", ");
    let setup = "CREATE TABLE t (a int, b int); CREATE TABLE pk (a int PRIMARY KEY);";
    let too_many_index = "cannot use more than 32 columns in an index";
    assert_rejected(
        setup,
        &[
            (
                &format!("CREATE INDEX ON t ({});", cols(33)),
                too_many_index,
            ),
            (
                &format!("CREATE INDEX ON t ({}) INCLUDE (a, b);", cols(31)),
                too_many_index,
            ),
            (
                &format!(
                    "CREATE TABLE u (a int, b int, UNIQUE (a) INCLUDE ({}));",
                    cols(32)
                ),
                too_many_index,
            ),
            (
                &format!("CREATE TABLE p (a int) PARTITION BY RANGE ({});", cols(33)),
                "cannot partition using more than 32 columns",
            ),
            (
                &format!("CREATE TABLE p (a int) PARTITION BY LIST ({});", cols(33)),
                "cannot partition using more than 32 columns",
            ),
            (
                &format!(
                    "CREATE TABLE f (a int, FOREIGN KEY ({}) REFERENCES pk);",
                    cols(33)
                ),
                "cannot have more than 32 keys in a foreign key",
            ),
        ],
    );
    assert_accepted(setup, &[&format!("CREATE INDEX ON t ({});", cols(32))]);
}

#[test]
fn concurrent_index_builds_and_drops_follow_pg_restrictions() {
    let setup = "CREATE TABLE t (a int, b int); CREATE INDEX i1 ON t (a); \
                 CREATE INDEX i2 ON t (b); \
                 CREATE TABLE p (a int) PARTITION BY RANGE (a); CREATE INDEX pi ON p (a);";
    assert_rejected(
        setup,
        &[
            (
                "DROP INDEX CONCURRENTLY i1, i2;",
                "DROP INDEX CONCURRENTLY does not support dropping multiple objects",
            ),
            (
                "DROP INDEX CONCURRENTLY i1 CASCADE;",
                "DROP INDEX CONCURRENTLY does not support CASCADE",
            ),
            (
                "DROP INDEX CONCURRENTLY pi;",
                "cannot drop partitioned index \"pi\" concurrently",
            ),
            (
                "CREATE INDEX CONCURRENTLY ON p (a);",
                "cannot create index on partitioned table \"p\" concurrently",
            ),
            (
                "CREATE INDEX CONCURRENTLY ON p USING nosuch (a);",
                "cannot create index on partitioned table \"p\" concurrently",
            ),
        ],
    );
    assert_accepted(
        setup,
        &[
            "DROP INDEX CONCURRENTLY i1;",
            "DROP INDEX CONCURRENTLY IF EXISTS nosuch;",
            "CREATE INDEX CONCURRENTLY ON t (a, b);",
        ],
    );
}

#[test]
fn an_index_cannot_use_a_table_access_method() {
    // GetIndexAmRoutine: heap_tableam_handler (oid 3) returns no
    // IndexAmRoutine.
    assert_rejected(
        "CREATE TABLE t (a int); \
         CREATE ACCESS METHOD myheap TYPE TABLE HANDLER heap_tableam_handler;",
        &[
            (
                "CREATE INDEX ON t USING heap (a);",
                "index access method handler function 3 did not return an IndexAmRoutine struct",
            ),
            (
                "CREATE INDEX ON t USING myheap (a);",
                "index access method handler function 3 did not return an IndexAmRoutine struct",
            ),
        ],
    );
}

#[test]
fn alter_column_type_resolves_index_operator_classes_again() {
    // ATPostAlterTypeCleanup rebuilds each index from pg_get_indexdef,
    // which names an operator class only when it isn't the old type's
    // default.
    let setup = "CREATE TABLE t (a varchar, b int, c int, d int, e text, f text, g int, h int);
                 CREATE INDEX ON t (a); CREATE INDEX ON t (b int4_ops);
                 CREATE INDEX ON t (c) INCLUDE (d); CREATE INDEX ON t ((h + 1));
                 CREATE INDEX ON t ((COALESCE(g, g)));
                 CREATE INDEX ON t (e text_pattern_ops);
                 CREATE INDEX ON t (f COLLATE \"C\");
                 CREATE TABLE u (a int UNIQUE, b int, EXCLUDE USING hash (b WITH =));";
    let no_btree = "data type point has no default operator class for access method \"btree\"";
    assert_rejected(
        setup,
        &[
            (
                "ALTER TABLE t ALTER b TYPE point USING point(b, b);",
                no_btree,
            ),
            (
                "ALTER TABLE t ALTER c TYPE point USING point(c, c);",
                no_btree,
            ),
            (
                "ALTER TABLE t ALTER g TYPE point USING point(g, g);",
                no_btree,
            ),
            (
                "ALTER TABLE t ALTER h TYPE point USING point(h, h);",
                "operator does not exist: point + integer",
            ),
            (
                "ALTER TABLE t ALTER e TYPE int USING 1;",
                "operator class \"text_pattern_ops\" does not accept data type integer",
            ),
            (
                "ALTER TABLE t ALTER f TYPE int USING 1;",
                "collations are not supported by type integer",
            ),
            (
                "ALTER TABLE u ALTER a TYPE point USING point(a, a);",
                no_btree,
            ),
            (
                "ALTER TABLE u ALTER b TYPE point USING point(b, b);",
                "data type point has no default operator class for access method \"hash\"",
            ),
        ],
    );
    assert_accepted(
        setup,
        &[
            "ALTER TABLE t ALTER a TYPE int USING a::int;",
            "ALTER TABLE t ALTER b TYPE bigint;",
            "ALTER TABLE t ALTER d TYPE point USING point(d, d);",
            "ALTER TABLE t ALTER e TYPE varchar;",
            "ALTER TABLE u ALTER b TYPE text;",
            "ALTER TABLE t ALTER g TYPE bigint; ALTER TABLE t ALTER h TYPE numeric;",
            "ALTER TABLE t ALTER a TYPE int USING a::int; ALTER TABLE t ALTER a TYPE text; \
             ALTER TABLE t ALTER e TYPE varchar; ALTER TABLE t ALTER e TYPE text;",
        ],
    );
}

#[test]
fn add_constraint_using_index_checks_the_index() {
    // transformIndexConstraint / ATExecAddIndexConstraint /
    // index_check_primary_key.
    let setup = "CREATE TABLE t (a int, b text);
                 CREATE UNIQUE INDEX i1 ON t (a) WHERE a > 0;
                 CREATE UNIQUE INDEX i2 ON t ((a + 1));
                 CREATE UNIQUE INDEX i3 ON t (a DESC);
                 CREATE UNIQUE INDEX i4 ON t (b text_pattern_ops);
                 CREATE UNIQUE INDEX i5 ON t (a) NULLS NOT DISTINCT;
                 CREATE UNIQUE INDEX i6 ON t (b COLLATE \"C\");
                 CREATE UNIQUE INDEX i7 ON t (a NULLS FIRST);
                 CREATE UNIQUE INDEX i9 ON t (b COLLATE \"default\");
                 CREATE UNIQUE INDEX i10 ON t (a int4_ops);
                 CREATE UNIQUE INDEX i11 ON t (a) INCLUDE (b);
                 CREATE INDEX nu ON t (a);
                 CREATE TABLE t2 (a int UNIQUE);
                 CREATE TABLE pp (a int) PARTITION BY RANGE (a);
                 CREATE UNIQUE INDEX pi ON pp (a);";
    let sorting =
        |i: &str| format!("index \"{i}\" column number 1 does not have default sorting behavior");
    let (s3, s4, s6, s7) = (sorting("i3"), sorting("i4"), sorting("i6"), sorting("i7"));
    assert_rejected(
        setup,
        &[
            (
                "ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i1;",
                "\"i1\" is a partial index",
            ),
            (
                "ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i2;",
                "index \"i2\" contains expressions",
            ),
            ("ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i3;", &s3),
            ("ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i4;", &s4),
            ("ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i6;", &s6),
            ("ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i7;", &s7),
            (
                "ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX nu;",
                "\"nu\" is not a unique index",
            ),
            (
                "ALTER TABLE t2 ADD CONSTRAINT u UNIQUE USING INDEX t2_a_key;",
                "index \"t2_a_key\" is already associated with a constraint",
            ),
            (
                "ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX t2_a_key;",
                "index \"t2_a_key\" is already associated with a constraint",
            ),
            (
                "ALTER TABLE pp ADD CONSTRAINT u UNIQUE USING INDEX pi;",
                "ALTER TABLE / ADD CONSTRAINT USING INDEX is not supported on partitioned tables",
            ),
            (
                "ALTER TABLE t ADD CONSTRAINT u PRIMARY KEY USING INDEX i5;",
                "primary keys cannot use NULLS NOT DISTINCT indexes",
            ),
        ],
    );
    assert_accepted(
        setup,
        &[
            "ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i9;",
            "ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i10;",
            "ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i11;",
            "ALTER TABLE t ADD CONSTRAINT u UNIQUE USING INDEX i5;",
            "ALTER TABLE t ADD CONSTRAINT u PRIMARY KEY USING INDEX i10;",
        ],
    );
}
