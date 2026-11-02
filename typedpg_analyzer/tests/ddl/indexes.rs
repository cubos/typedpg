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
fn index_expressions_reject_record() {
    // index_create's ConstructTupleDescriptor runs CheckAttributeType on
    // each expression column, named as ChooseIndexColumnNames names it:
    // `record` has operator classes, but isn't storable.
    assert_rejected(
        "CREATE TABLE fih (f1 text, f2 text);",
        &[
            (
                "CREATE INDEX ON fih ((f1 || f2), (row(f1, f2)));",
                r#"column "row" has pseudo-type record"#,
            ),
            (
                "CREATE INDEX ix ON fih (f1, (row(f1, f2)::record));",
                r#"column "row" has pseudo-type record"#,
            ),
            (
                "CREATE INDEX ON fih ((ARRAY[row(f1, f2)]));",
                r#"column "array" has pseudo-type record[]"#,
            ),
            (
                "ALTER TABLE fih ADD EXCLUDE ((row(f1, f2)) WITH =);",
                r#"column "row" has pseudo-type record"#,
            ),
        ],
    );
    assert_accepted(
        "CREATE TABLE fih (f1 int, f2 int); CREATE TYPE pr AS (x int, y int);",
        &[
            "CREATE INDEX ON fih ((row(f1, f2)::pr));",
            "CREATE INDEX ON fih ((ARRAY[f1, f2]));",
        ],
    );
}

#[test]
fn unnamed_indexes_are_named_after_their_columns() {
    // ChooseIndexColumnNames: an expression column is named as a SELECT
    // would name it (FigureColnameInternal), else `expr`; repeated names
    // are numbered. Names from PG 18.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int, c text);
         CREATE INDEX ON t ((CASE WHEN a > 0 THEN b END), ('x'::text), (coalesce(a, b)),
             (c::varchar), (ARRAY[a]), (greatest(a, b)), (nullif(a, b)), (a + 1));
         CREATE INDEX ON t ((lower(c)), (lower(c)), (a + 1), (a + 2));",
    )]);
    let mut names: Vec<String> = user_indexes(&db)
        .into_iter()
        .map(|i| db.pg_class()[&i.indexrelid].relname.clone())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "t_case_text_coalesce_c_array_greatest_nullif_expr_idx",
            "t_lower_lower1_expr_expr1_idx",
        ]
    );
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

#[test]
fn attach_a_partition_index_by_name() {
    // DefineIndex's recursion adopts a matching index of the partition —
    // a plain index takes a constraint's (tp_pkey), whose constraint stays
    // local — and ATTACH PARTITION of the adopted one is a no-op.
    let setup = "CREATE TABLE t (a integer, b integer) PARTITION BY HASH (a, b);
                 CREATE TABLE tp (a integer, b integer, PRIMARY KEY (a, b), UNIQUE (b, a));
                 ALTER TABLE t ATTACH PARTITION tp FOR VALUES WITH (MODULUS 1, REMAINDER 0);";
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE UNIQUE INDEX t_a_idx ON t (a, b);
             ALTER INDEX t_a_idx ATTACH PARTITION tp_pkey;",
        ),
    ]);
    let mut names: Vec<String> = user_indexes(&db)
        .into_iter()
        .map(|i| db.pg_class()[&i.indexrelid].relname.clone())
        .collect();
    names.sort();
    assert_eq!(names, ["t_a_idx", "tp_b_a_key", "tp_pkey"]);
    let con = db
        .constraints_of_table("tp")
        .into_iter()
        .find(|c| c.conname == "tp_pkey")
        .unwrap();
    assert!(con.conislocal && con.coninhcount == 0);
    // A constraint's index still needs a constraint's: t_pk gets a copy.
    assert_rejected(
        "CREATE TABLE t (a integer NOT NULL, b integer) PARTITION BY HASH (a);
         CREATE TABLE tp (a integer NOT NULL, b integer);
         CREATE UNIQUE INDEX tp_a ON tp (a);
         ALTER TABLE t ATTACH PARTITION tp FOR VALUES WITH (MODULUS 1, REMAINDER 0);
         ALTER TABLE t ADD CONSTRAINT t_pk PRIMARY KEY (a);",
        &[(
            "ALTER INDEX t_pk ATTACH PARTITION tp_a;",
            "cannot attach index \"tp_a\" as a partition of index \"t_pk\" (Another index is \
             already attached for partition \"tp\".)",
        )],
    );
}

#[test]
fn attach_a_matching_expression_index() {
    // CompareIndexInfo compares expressions and predicates as parse trees,
    // whatever their spelling.
    let setup = "CREATE TABLE idxpart (a int, b int) PARTITION BY RANGE (a);
                 CREATE TABLE idxpart1 PARTITION OF idxpart FOR VALUES FROM (0) TO (10);";
    assert_accepted(
        setup,
        &[
            "CREATE INDEX idxpart_expr_idx ON ONLY idxpart ((a + b));
             CREATE INDEX idxpart1_expr_idx ON idxpart1 ((a + b));
             ALTER INDEX idxpart_expr_idx ATTACH PARTITION idxpart1_expr_idx;",
            "CREATE INDEX ip ON ONLY idxpart (a) WHERE b > 0;
             CREATE INDEX ip1 ON idxpart1 (a) WHERE (b>0);
             ALTER INDEX ip ATTACH PARTITION ip1;",
        ],
    );
    assert_rejected(
        setup,
        &[(
            "CREATE INDEX idxpart_expr_idx ON ONLY idxpart ((a + b));
             CREATE INDEX idxpart1_expr_idx ON idxpart1 ((b + a));
             ALTER INDEX idxpart_expr_idx ATTACH PARTITION idxpart1_expr_idx;",
            "cannot attach index \"idxpart1_expr_idx\" as a partition of index \
             \"idxpart_expr_idx\" (The index definitions do not match.)",
        )],
    );
    // An index created on the parent adopts the partition's equal one.
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE INDEX idxpart1_expr_idx ON idxpart1 ((a + b));
             CREATE INDEX idxpart_expr_idx ON idxpart ((a+b));",
        ),
    ]);
    assert_eq!(user_indexes(&db).len(), 2);
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
fn unknown_typed_index_keys_have_no_operator_class() {
    // ComputeIndexAttrs runs ResolveOpClass before index_create checks the
    // key's type (CheckAttributeType): `unknown` — an untyped literal, NULL
    // or a cast to it — has no operator class, nor a collation.
    let no_default = |am: &str| {
        format!("data type unknown has no default operator class for access method \"{am}\"")
    };
    let btree = no_default("btree");
    let setup = "CREATE TABLE t (a int, b text, c text);";
    for (sql, message) in [
        ("CREATE INDEX ON t (('a'));", btree.as_str()),
        ("CREATE INDEX ON t (('a'::text::unknown));", &btree),
        ("CREATE INDEX ON t ((b::unknown));", &btree),
        ("CREATE INDEX ON t ((NULL));", &btree),
        ("CREATE INDEX ON t ((unknownin('x')));", &btree),
        ("CREATE INDEX ON t (((ROW('a')).f1));", &btree),
        ("CREATE INDEX ON t ((b::unknown) DESC);", &btree),
        (
            "CREATE INDEX ON t USING hash ((b::unknown));",
            &no_default("hash"),
        ),
        // ResolveOpClass comes before the ordering options.
        (
            "CREATE INDEX ON t USING hash ((b::unknown) DESC);",
            &no_default("hash"),
        ),
        (
            "CREATE INDEX ON t USING gist ((b::unknown));",
            &no_default("gist"),
        ),
        (
            "CREATE INDEX ON t USING brin ((b::unknown));",
            &no_default("brin"),
        ),
        (
            "CREATE INDEX ON t ((b::unknown) text_ops);",
            "operator class \"text_ops\" does not accept data type unknown",
        ),
        (
            "CREATE INDEX ON t ((b::unknown) nosuch_ops);",
            "operator class \"nosuch_ops\" does not exist for access method \"btree\"",
        ),
        (
            "CREATE INDEX ON t ((b::unknown) COLLATE \"C\");",
            "collations are not supported by type unknown",
        ),
        (
            "CREATE INDEX ON t (('a') COLLATE \"C\");",
            "collations are not supported by type unknown",
        ),
        // An exclusion constraint's index: the operator class before the
        // exclusion operator.
        (
            "ALTER TABLE t ADD EXCLUDE USING btree ((c::unknown) WITH =);",
            &btree,
        ),
        (
            "ALTER TABLE t ADD EXCLUDE USING btree (('x') WITH =);",
            &btree,
        ),
        (
            "ALTER TABLE t ADD EXCLUDE USING btree ((c::unknown) WITH <>);",
            &btree,
        ),
        (
            "ALTER TABLE t ADD EXCLUDE USING btree ((c::unknown) text_ops WITH =);",
            "operator class \"text_ops\" does not accept data type unknown",
        ),
        (
            "CREATE TABLE t2 (a int, EXCLUDE USING btree ((NULL) WITH =));",
            &btree,
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", sql)]).expect_err(sql);
        assert!(
            err.to_string().starts_with(message),
            "{sql}\n  expected: {message}\n       got: {err}"
        );
    }
    // A NULL CASE resolves to text; a record key is CheckAttributeType's.
    assert_accepted(
        setup,
        &["CREATE INDEX ON t ((CASE WHEN a > 0 THEN NULL END));"],
    );
    assert_rejected(
        setup,
        &[(
            "CREATE INDEX ON t ((ROW('a')));",
            "column \"row\" has pseudo-type record",
        )],
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
fn brin_bloom_operator_class_options() {
    // brin_bloom_options: two real options, n_distinct_per_range and
    // false_positive_rate.
    let setup = "CREATE TABLE bb (b bytea);";
    assert_accepted(
        setup,
        &[
            "CREATE INDEX ON bb USING brin (b bytea_bloom_ops(n_distinct_per_range = 100));",
            "CREATE INDEX ON bb USING brin \
             (b bytea_bloom_ops(false_positive_rate = 0.05, n_distinct_per_range = -0.5));",
        ],
    );
    assert_rejected(
        setup,
        &[
            (
                "CREATE INDEX ON bb USING brin (b bytea_bloom_ops(n_distinct_per_range = -2));",
                "value -2 out of bounds for option \"n_distinct_per_range\" (Valid values are \
                 between \"-1.000000\" and \"2147483647.000000\".)",
            ),
            (
                "CREATE INDEX ON bb USING brin (b bytea_bloom_ops(false_positive_rate = 1e-5));",
                "value 1e-5 out of bounds for option \"false_positive_rate\" (Valid values are \
                 between \"0.000100\" and \"0.250000\".)",
            ),
            (
                "CREATE INDEX ON bb USING brin (b bytea_bloom_ops(false_positive_rate = 'x'));",
                "invalid value for floating point option \"false_positive_rate\": x",
            ),
            (
                "CREATE INDEX ON bb USING brin (b bytea_bloom_ops(false_positive_rate));",
                "invalid value for floating point option \"false_positive_rate\": true",
            ),
            (
                "CREATE INDEX ON bb USING brin (b bytea_bloom_ops(siglen = 1));",
                "unrecognized parameter \"siglen\"",
            ),
            (
                "CREATE INDEX ON bb USING brin (b bytea_minmax_ops(n_distinct_per_range = 1));",
                "operator class bytea_minmax_ops has no options",
            ),
        ],
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
fn exclusion_expressions_and_predicates_are_checked_like_an_index() {
    // transformIndexConstraint makes them the index's expressions and
    // predicate: EXPR_KIND_INDEX_EXPRESSION / _PREDICATE, IMMUTABLE only,
    // a boolean WHERE.
    assert_rejected(
        "",
        &[
            (
                "CREATE TABLE t (a int, EXCLUDE (a WITH =) WHERE (generate_series(1, a) > 0));",
                "set-returning functions are not allowed in index predicates",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE (a WITH =) WHERE (random() > 0.5));",
                "functions in index predicate must be marked IMMUTABLE",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE ((random()::int) WITH =));",
                "functions in index expression must be marked IMMUTABLE",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE (a WITH =) WHERE (a));",
                "argument of WHERE must be type boolean, not type integer",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE ((sum(a)) WITH =));",
                "aggregate functions are not allowed in index expressions",
            ),
            (
                "CREATE TABLE t (a int, EXCLUDE (a WITH =) WHERE (nosuch > 0));",
                "column \"nosuch\" does not exist",
            ),
        ],
    );
    assert_accepted(
        "",
        &["CREATE TABLE t (a int, EXCLUDE ((a + 1) WITH =) WHERE (a > 0));"],
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
fn alter_index_and_friends_need_a_relation_of_their_kind() {
    // RangeVarCallbackForAlterRelation (42809).
    assert_rejected(
        "CREATE TABLE t (a int); CREATE VIEW v AS SELECT 1 AS a;",
        &[
            (
                "ALTER INDEX t SET (fillfactor = 50);",
                "\"t\" is not an index",
            ),
            (
                "ALTER INDEX v ALTER COLUMN a SET STATISTICS 5;",
                "\"v\" is not an index",
            ),
            ("ALTER VIEW t OWNER TO CURRENT_USER;", "\"t\" is not a view"),
            (
                "ALTER MATERIALIZED VIEW t OWNER TO CURRENT_USER;",
                "\"t\" is not a materialized view",
            ),
            (
                "ALTER FOREIGN TABLE t OWNER TO CURRENT_USER;",
                "\"t\" is not a foreign table",
            ),
            (
                "ALTER SEQUENCE t OWNER TO CURRENT_USER;",
                "\"t\" is not a sequence",
            ),
        ],
    );
    assert_accepted(
        "CREATE TABLE t (a int); CREATE INDEX i ON t (a);",
        &[
            "ALTER INDEX i SET (fillfactor = 50);",
            "ALTER INDEX t RENAME TO t2;",
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
fn cluster_needs_an_access_method_that_supports_clustering() {
    let setup = "CREATE TABLE t (a int); CREATE INDEX h ON t USING hash (a); \
                 CREATE INDEX br ON t USING brin (a); \
                 CREATE INDEX g ON t USING gist (point(a, a)); CREATE INDEX b ON t (a);";
    let message = |name: &str| {
        format!(
            "cannot cluster on index \"{name}\" because access method does not support clustering"
        )
    };
    let (h, br) = (message("h"), message("br"));
    assert_rejected(
        setup,
        &[
            ("ALTER TABLE t CLUSTER ON h;", &h),
            ("CLUSTER t USING h;", &h),
            ("ALTER TABLE t CLUSTER ON br;", &br),
        ],
    );
    assert_accepted(
        setup,
        &["ALTER TABLE t CLUSTER ON g;", "CLUSTER t USING b;"],
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

/// `(indclass as opclass names, indcollation as collation names)` of the
/// index named `name`.
fn index_classes(db: &PgCatalog, name: &str) -> (Vec<Option<String>>, Vec<Option<String>>) {
    let seed = db.to_seed();
    let index = db
        .pg_index_values()
        .find(|i| {
            db.pg_class()
                .get(&i.indexrelid)
                .is_some_and(|c| c.relname == name)
        })
        .unwrap_or_else(|| panic!("no index {name}"));
    let classes = index
        .indclass
        .iter()
        .map(|c| {
            c.and_then(|c| seed.pg_opclass.iter().find(|o| o.oid == c))
                .map(|o| o.opcname.clone())
        })
        .collect();
    let collations = index
        .indcollation
        .iter()
        .map(|c| {
            c.and_then(|c| seed.pg_collation.iter().find(|o| o.oid == c))
                .map(|o| o.collname.clone())
        })
        .collect();
    (classes, collations)
}

#[test]
fn indexes_record_their_operator_classes_and_collations() {
    // pg_index.indclass / indcollation, per key column, for every way an
    // index comes to be.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b text, c text COLLATE \"C\", d varchar);
         CREATE INDEX i ON t (a, b COLLATE \"C\" text_pattern_ops, c, (a + 1), d) INCLUDE (b);
         ALTER TABLE t ADD CONSTRAINT u UNIQUE (b, c);
         CREATE TABLE e (r int4range, EXCLUDE USING gist (r WITH &&));
         CREATE TABLE l (LIKE t INCLUDING INDEXES);
         CREATE TABLE p (a int, b text) PARTITION BY RANGE (a);
         CREATE INDEX pi ON p (b COLLATE \"POSIX\" varchar_pattern_ops);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);",
    )]);
    let some = |v: &[&str]| -> Vec<Option<String>> {
        v.iter()
            .map(|s| (!s.is_empty()).then(|| (*s).to_owned()))
            .collect()
    };
    let i = (
        some(&[
            "int4_ops",
            "text_pattern_ops",
            "text_ops",
            "int4_ops",
            "text_ops",
        ]),
        some(&["", "C", "C", "", "default"]),
    );
    assert_eq!(index_classes(&db, "i"), i);
    assert_eq!(index_classes(&db, "l_a_b_c_expr_d_b1_idx"), i);
    let u = (some(&["text_ops", "text_ops"]), some(&["default", "C"]));
    assert_eq!(index_classes(&db, "u"), u);
    assert_eq!(index_classes(&db, "l_b_c_key"), u);
    assert_eq!(
        index_classes(&db, "e_r_excl"),
        (some(&["range_ops"]), some(&[""]))
    );
    let pi = (some(&["varchar_pattern_ops"]), some(&["POSIX"]));
    assert_eq!(index_classes(&db, "pi"), pi);
    assert_eq!(index_classes(&db, "p1_b_idx"), pi);
    // ALTER COLUMN TYPE re-resolves them: a default operator class and
    // collation follow the new type, an explicit one stays.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b text);
         CREATE INDEX i ON t (a, b text_pattern_ops);
         ALTER TABLE t ALTER a TYPE text, ALTER b TYPE varchar;",
    )]);
    assert_eq!(
        index_classes(&db, "i"),
        (
            some(&["text_ops", "text_pattern_ops"]),
            some(&["default", "default"])
        )
    );
}

#[test]
fn partition_indexes_match_on_collation_and_operator_family() {
    // CompareIndexInfo compares the key columns' collations and operator
    // families: ATTACH PARTITION adopts only a matching index (a
    // varchar_pattern_ops one for text_pattern_ops — same family), and
    // ALTER INDEX ... ATTACH PARTITION refuses the others.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int, b text) PARTITION BY RANGE (a);
         CREATE INDEX pi ON p (b COLLATE \"C\");
         CREATE INDEX pj ON p (b text_pattern_ops);
         CREATE TABLE p1 (a int, b text);
         CREATE INDEX p1_b ON p1 (b);
         CREATE INDEX p1_bp ON p1 (b varchar_pattern_ops);
         ALTER TABLE p ATTACH PARTITION p1 FOR VALUES FROM (0) TO (10);",
    )]);
    let mut names: Vec<String> = db
        .pg_index_values()
        .filter(|i| {
            db.pg_class()
                .get(&i.indrelid)
                .is_some_and(|c| c.relname == "p1")
        })
        .map(|i| db.pg_class()[&i.indexrelid].relname.clone())
        .collect();
    names.sort();
    assert_eq!(names, ["p1_b", "p1_b_idx", "p1_bp"]);
    let setup = "CREATE TABLE q (a int, b text) PARTITION BY RANGE (a);
                 CREATE TABLE q1 PARTITION OF q FOR VALUES FROM (0) TO (10);
                 CREATE INDEX q1_b ON q1 (b);
                 CREATE INDEX q1_bp ON q1 (b varchar_pattern_ops);
                 CREATE INDEX qi ON ONLY q (b COLLATE \"C\");
                 CREATE INDEX qj ON ONLY q (b text_pattern_ops);";
    assert_rejected(
        setup,
        &[
            (
                "ALTER INDEX qi ATTACH PARTITION q1_b;",
                "cannot attach index \"q1_b\" as a partition of index \"qi\"",
            ),
            (
                "ALTER INDEX qj ATTACH PARTITION q1_b;",
                "cannot attach index \"q1_b\" as a partition of index \"qj\"",
            ),
        ],
    );
    assert_accepted(setup, &["ALTER INDEX qj ATTACH PARTITION q1_bp;"]);
}

#[test]
fn unique_constraints_cover_every_level_of_partition_keys() {
    // DefineIndex recurses into a partitioned partition with its own
    // partition key, which the copy must cover too — as must the copies
    // ATTACH PARTITION and PARTITION OF make.
    let setup = "CREATE EXTENSION btree_gist;
                 CREATE TABLE ip (a int, b int) PARTITION BY RANGE (a);
                 CREATE TABLE ip2 PARTITION OF ip FOR VALUES FROM (0) TO (1000) \
                     PARTITION BY RANGE (b);
                 CREATE TABLE jp (a int, b int, UNIQUE (a)) PARTITION BY RANGE (a);
                 CREATE TABLE kp (a int, b int) PARTITION BY RANGE (a);
                 CREATE TABLE kp1 PARTITION OF kp FOR VALUES FROM (0) TO (10) \
                     PARTITION BY LIST ((b + 1));";
    let lacks = "unique constraint on partitioned table must include all partitioning columns";
    assert_rejected(
        setup,
        &[
            ("ALTER TABLE ip ADD PRIMARY KEY (a);", lacks),
            ("ALTER TABLE ip ADD UNIQUE (a);", lacks),
            ("CREATE UNIQUE INDEX ON ip (a);", lacks),
            ("ALTER TABLE ip ADD EXCLUDE USING gist (a WITH =);", lacks),
            (
                "CREATE TABLE jp2 PARTITION OF jp FOR VALUES FROM (10) TO (20) \
                 PARTITION BY LIST (b);",
                lacks,
            ),
            (
                "CREATE TABLE ip3 (a int, b int) PARTITION BY LIST (b);
                 ALTER TABLE jp ATTACH PARTITION ip3 FOR VALUES FROM (0) TO (10);",
                lacks,
            ),
            (
                "ALTER TABLE kp ADD PRIMARY KEY (a);",
                "unsupported PRIMARY KEY constraint with partition key definition",
            ),
        ],
    );
    assert_accepted(
        setup,
        &[
            "ALTER TABLE ip ADD PRIMARY KEY (a, b);",
            "CREATE UNIQUE INDEX ON ONLY ip (a);",
            "ALTER TABLE ip ADD EXCLUDE USING gist (a WITH =, b WITH =);",
            "CREATE TABLE jp3 PARTITION OF jp FOR VALUES FROM (20) TO (30) PARTITION BY LIST (a);",
        ],
    );
}

#[test]
fn unique_index_covers_the_partition_key_only_with_its_equality() {
    // DefineIndex: the index column's operator family must have the
    // partition key's equality operator (record_image_ops' is *=, not =).
    let setup = "CREATE TYPE pr AS (x int);
                 CREATE TABLE t (a pr, b text) PARTITION BY LIST (a);
                 CREATE TABLE h (a int) PARTITION BY HASH (a);";
    assert_rejected(
        setup,
        &[(
            "CREATE UNIQUE INDEX ON t (a record_image_ops);",
            "unique constraint on partitioned table must include all partitioning columns",
        )],
    );
    assert_accepted(
        setup,
        &[
            "CREATE UNIQUE INDEX ON t (a);",
            "CREATE UNIQUE INDEX ON t (a record_ops, b text_pattern_ops);",
            "CREATE UNIQUE INDEX ON h (a);",
        ],
    );
}

#[test]
fn an_adopted_constraint_index_goes_with_its_parent_index() {
    // A plain partitioned index that adopted a partition's constraint
    // index holds it (DEPENDENCY_PARTITION_PRI): the constraint can't be
    // dropped alone, nor the parent index without CASCADE. Errors from
    // PG 18.6.
    let setup = "CREATE TABLE b (a int, b int) PARTITION BY RANGE (a);
                 CREATE TABLE bp (a int, b int, CONSTRAINT k UNIQUE (a));
                 ALTER TABLE b ATTACH PARTITION bp FOR VALUES FROM (0) TO (5);
                 CREATE UNIQUE INDEX bi ON b (a);";
    assert_rejected(
        setup,
        &[
            (
                "ALTER TABLE bp DROP CONSTRAINT k;",
                "cannot drop index k because index bi requires it (You can drop index bi \
                 instead.)",
            ),
            (
                "DROP INDEX bi;",
                "cannot drop index bi because other objects depend on it (constraint k on \
                 table bp depends on index bi)",
            ),
            (
                "DROP INDEX k;",
                "cannot drop index k because index bi requires it (You can drop index bi \
                 instead.)",
            ),
        ],
    );
    assert_accepted(
        setup,
        &[
            "ALTER TABLE b DETACH PARTITION bp; ALTER TABLE bp DROP CONSTRAINT k;",
            // Another table's constraint of the same name has no index.
            "CREATE TABLE c (x int, CONSTRAINT k FOREIGN KEY (x) REFERENCES bp (a));
             ALTER TABLE c DROP CONSTRAINT k;",
        ],
    );
    // CASCADE takes the constraint along.
    let db = build_db(&[("0001.sql", setup), ("0002.sql", "DROP INDEX bi CASCADE;")]);
    assert!(user_indexes(&db).is_empty());
    assert!(
        db.constraints_of_table("bp")
            .iter()
            .all(|c| c.conname != "k")
    );
}

/// Apply `sql` to `db`, expecting it to fail with a message starting with
/// `message`.
fn expect_rejected(db: &mut PgCatalog, sql: &str, message: &str) {
    let err = db.apply_sql(sql).expect_err(sql);
    assert!(
        err.to_string().starts_with(message),
        "{sql}\n  expected: {message}\n       got: {err}"
    );
}

#[test]
fn index_columns_are_renamed_with_alter_table_rename_column() {
    // renameatt accepts an index (RangeVarCallbackForRenameAttribute):
    // the index column is the index's own, which statements naming an
    // index column see afterwards.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int PRIMARY KEY, b int, c int);
         CREATE INDEX t_b ON t (b) INCLUDE (c);
         CREATE INDEX t_e ON t ((a + b), (a - b), a, a);",
    )]);
    db.apply_sql("ALTER TABLE t_pkey RENAME COLUMN a TO y;")
        .unwrap();
    expect_rejected(
        &mut db,
        "ALTER INDEX t_pkey ALTER COLUMN a SET STATISTICS 10;",
        "column \"a\" of relation \"t_pkey\" does not exist",
    );
    expect_rejected(
        &mut db,
        "ALTER INDEX t_pkey ALTER COLUMN y SET STATISTICS 10;",
        "cannot alter statistics on non-expression column \"y\" of index \"t_pkey\"",
    );
    // An index column may collide with no other of the index, but takes a
    // system column's name (an index has none).
    expect_rejected(
        &mut db,
        "ALTER TABLE t_pkey RENAME COLUMN y TO y;",
        "column \"y\" of relation \"t_pkey\" already exists",
    );
    expect_rejected(
        &mut db,
        "ALTER TABLE t_pkey RENAME COLUMN a TO z;",
        "column \"a\" does not exist",
    );
    db.apply_sql("ALTER TABLE IF EXISTS t_pkey RENAME COLUMN y TO ctid;")
        .unwrap();
    db.apply_sql("ALTER TABLE t_b RENAME COLUMN c TO z;")
        .unwrap();
    expect_rejected(
        &mut db,
        "ALTER INDEX t_b ALTER COLUMN z SET STATISTICS 5;",
        "cannot alter statistics on included column \"z\" of index \"t_b\"",
    );
    expect_rejected(
        &mut db,
        "COMMENT ON COLUMN t_b.c IS 'x';",
        "column \"c\" of relation \"t_b\" does not exist",
    );
    expect_rejected(
        &mut db,
        "COMMENT ON COLUMN t_b.z IS 'x';",
        "cannot set comment on relation \"t_b\"",
    );
    // Expression columns: `expr`, `expr1`, then the table's names
    // deduplicated (`a`, `a1`).
    expect_rejected(
        &mut db,
        "ALTER TABLE t_e RENAME COLUMN expr TO a1;",
        "column \"a1\" of relation \"t_e\" already exists",
    );
    db.apply_sql(
        "ALTER TABLE t_e RENAME COLUMN expr1 TO e;
         ALTER INDEX t_e ALTER COLUMN e SET STATISTICS 5;
         ALTER INDEX t_e ALTER COLUMN 2 SET STATISTICS 5;",
    )
    .unwrap();
    expect_rejected(
        &mut db,
        "ALTER INDEX t_e ALTER COLUMN expr1 SET STATISTICS 5;",
        "column \"expr1\" of relation \"t_e\" does not exist",
    );
}

#[test]
fn renaming_a_table_column_keeps_its_index_columns_names() {
    // PG never renames an index column after its table's — until ALTER
    // COLUMN TYPE rebuilds the index from pg_get_indexdef, which names the
    // table's columns as they are.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int, c int);
         CREATE INDEX t_ab ON t (a, b);
         CREATE INDEX t_inc ON t (a) INCLUDE (c);
         CREATE INDEX t_pred ON t (a) WHERE c > 0;
         CREATE INDEX t_other ON t (a);",
    )]);
    db.apply_sql("ALTER TABLE t RENAME COLUMN b TO bb; ALTER TABLE t RENAME COLUMN a TO aa;")
        .unwrap();
    expect_rejected(
        &mut db,
        "ALTER INDEX t_ab ALTER COLUMN bb SET STATISTICS 5;",
        "column \"bb\" of relation \"t_ab\" does not exist",
    );
    db.apply_sql(
        "ALTER TABLE t_ab RENAME COLUMN a TO q;
         ALTER TABLE t ALTER COLUMN bb TYPE bigint;",
    )
    .unwrap();
    // Rebuilt: both of t_ab's columns named after the table's again.
    expect_rejected(
        &mut db,
        "ALTER TABLE t_ab RENAME COLUMN q TO z;",
        "column \"q\" does not exist",
    );
    db.apply_sql(
        "ALTER TABLE t_ab RENAME COLUMN aa TO a2; ALTER TABLE t_ab RENAME COLUMN bb TO b2;",
    )
    .unwrap();
    // An INCLUDE column or the predicate reading the column rebuilds the
    // index too; an index not reading it keeps its names.
    db.apply_sql("ALTER TABLE t RENAME COLUMN c TO cc; ALTER TABLE t ALTER COLUMN cc TYPE bigint;")
        .unwrap();
    db.apply_sql(
        "ALTER TABLE t_inc RENAME COLUMN cc TO c3;
         ALTER TABLE t_pred RENAME COLUMN aa TO a3;
         ALTER TABLE t_other RENAME COLUMN a TO a4;",
    )
    .unwrap();
}

#[test]
fn partitioned_index_columns_are_renamed_in_every_partition() {
    // A partitioned index's partitions are its inheritors: renameatt
    // renames the column in each first, or refuses with ONLY. Index
    // columns are never inherited (attinhcount 0), so a partition's may be
    // renamed on its own.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int, b int) PARTITION BY RANGE (a);
         CREATE INDEX p_b ON p (b);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (1) TO (2) PARTITION BY RANGE (a);
         CREATE TABLE p11 PARTITION OF p1 FOR VALUES FROM (1) TO (2);",
    )]);
    expect_rejected(
        &mut db,
        "ALTER TABLE ONLY p_b RENAME COLUMN b TO z;",
        "inherited column \"b\" must be renamed in child tables too",
    );
    db.apply_sql("ALTER TABLE p11_b_idx RENAME COLUMN b TO direct;")
        .unwrap();
    expect_rejected(
        &mut db,
        "ALTER TABLE p_b RENAME COLUMN b TO z;",
        "column \"b\" does not exist",
    );
    db.apply_sql(
        "ALTER TABLE p11_b_idx RENAME COLUMN direct TO b;
         ALTER TABLE p_b RENAME COLUMN b TO z;
         ALTER TABLE p11_b_idx RENAME COLUMN z TO w;
         CREATE TABLE p2 (a int, b int);
         CREATE INDEX p2_b ON p2 (b);
         ALTER TABLE p2_b RENAME COLUMN b TO qq;
         ALTER TABLE p ATTACH PARTITION p2 FOR VALUES FROM (2) TO (3);",
    )
    .unwrap();
    // The attached index keeps its own name.
    expect_rejected(
        &mut db,
        "ALTER TABLE p_b RENAME COLUMN z TO k;",
        "column \"z\" does not exist",
    );
    db.apply_sql(
        "ALTER TABLE p2_b RENAME COLUMN qq TO z;
         ALTER TABLE p11_b_idx RENAME COLUMN w TO z;
         ALTER TABLE p_b RENAME COLUMN z TO k;
         ALTER TABLE p1_b_idx RENAME COLUMN k TO j;",
    )
    .unwrap();
}

#[test]
fn renaming_columns_needs_a_relation_with_renameable_columns() {
    // renameatt_check: not a sequence's or a TOAST table's columns, nor a
    // system catalog's (or its index's).
    let mut db = build_db(&[("0001.sql", "CREATE SEQUENCE s;")]);
    expect_rejected(
        &mut db,
        "ALTER TABLE s RENAME COLUMN last_value TO lv;",
        "cannot rename columns of relation \"s\"",
    );
    expect_rejected(
        &mut db,
        "ALTER TABLE pg_class RENAME COLUMN relname TO rn;",
        "permission denied: \"pg_class\" is a system catalog",
    );
    expect_rejected(
        &mut db,
        "ALTER TABLE pg_class_oid_index RENAME COLUMN oid TO x;",
        "permission denied: \"pg_class_oid_index\" is a system catalog",
    );
    expect_rejected(
        &mut db,
        "ALTER TABLE pg_toast.pg_toast_2620 RENAME COLUMN chunk_id TO x;",
        "cannot rename columns of relation \"pg_toast_2620\"",
    );
}

#[test]
fn cloned_indexes_take_their_source_index_columns_names() {
    // generateClonedIndexStmt copies the source index's column names
    // (indexcolname): the clone keeps them, and ChooseIndexName names it
    // after them — a partition's index (PARTITION OF, ATTACH PARTITION
    // building one, a primary key or unique constraint's) and a LIKE ...
    // INCLUDING INDEXES copy alike.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int, b int) PARTITION BY RANGE (a);
         CREATE INDEX pi ON p ((a + b), b);
         CREATE INDEX pi2 ON p (b, (a + b));
         CREATE TABLE pp (a int, b int) PARTITION BY RANGE (a);
         CREATE INDEX ppi ON pp (b);
         CREATE TABLE s (a int PRIMARY KEY, b int UNIQUE, c int);
         CREATE INDEX si ON s (b) INCLUDE (c);
         CREATE TABLE pa (a int PRIMARY KEY, b int) PARTITION BY RANGE (a);
         CREATE TABLE pa1 PARTITION OF pa FOR VALUES FROM (1) TO (2);
         CREATE TABLE pu (a int, b int, UNIQUE (b)) PARTITION BY LIST (b);",
    )]);
    db.apply_sql(
        "ALTER TABLE pi RENAME COLUMN expr TO x;
         ALTER TABLE p RENAME COLUMN b TO bb;
         ALTER TABLE ppi RENAME COLUMN b TO bz;
         ALTER TABLE si RENAME COLUMN c TO cz;
         ALTER TABLE s_pkey RENAME COLUMN a TO ka;
         ALTER TABLE s_b_key RENAME COLUMN b TO kb;
         ALTER TABLE pa RENAME COLUMN a TO aa;
         ALTER TABLE pu_b_key RENAME COLUMN b TO kb;",
    )
    .unwrap();
    db.apply_sql(
        "CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (10) TO (20);
         ALTER TABLE p2_x_b_idx RENAME COLUMN x TO x2;
         ALTER TABLE pi2 RENAME COLUMN b TO bz;
         ALTER TABLE p2_b_expr_idx RENAME COLUMN bz TO q;",
    )
    .unwrap();
    // ATTACH PARTITION building the partition's index.
    db.apply_sql(
        "CREATE TABLE c1 (a int, b int);
         ALTER TABLE pp ATTACH PARTITION c1 FOR VALUES FROM (1) TO (2);
         ALTER TABLE c1_bz_idx RENAME COLUMN bz TO q;",
    )
    .unwrap();
    // LIKE ... INCLUDING INDEXES: INCLUDE columns and constraints' indexes
    // too (the constraint is named after the index columns).
    db.apply_sql(
        "CREATE TABLE s2 (LIKE s INCLUDING INDEXES);
         ALTER TABLE s2_b_cz_idx RENAME COLUMN cz TO q;
         ALTER TABLE s2_kb_key RENAME COLUMN kb TO q;
         ALTER TABLE s2_pkey RENAME COLUMN ka TO q;",
    )
    .unwrap();
    // A partition's primary key keeps the parent index's column name, not
    // the renamed table column's; a unique constraint's clone is named
    // after it.
    db.apply_sql("CREATE TABLE pa2 PARTITION OF pa FOR VALUES FROM (2) TO (3);")
        .unwrap();
    expect_rejected(
        &mut db,
        "ALTER TABLE pa2_pkey RENAME COLUMN aa TO z;",
        "column \"aa\" does not exist",
    );
    db.apply_sql(
        "ALTER TABLE pa2_pkey RENAME COLUMN a TO z;
         CREATE TABLE pu1 PARTITION OF pu FOR VALUES IN (1);
         ALTER TABLE pu1_kb_key RENAME COLUMN kb TO q;",
    )
    .unwrap();
}

#[test]
fn clones_copy_the_names_an_index_had_when_its_transaction_got_it() {
    // generateClonedIndexStmt reads the source index's relcache entry. One
    // created in the current transaction, or given a new relfilenode in it
    // (TRUNCATE, REINDEX, CLUSTER, an ALTER COLUMN TYPE rebuild), keeps its
    // entry, whose tuple descriptor isn't rebuilt: a rename in the same
    // transaction doesn't reach the clone.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE pp (a int, b int) PARTITION BY RANGE (a);
         CREATE INDEX ppi ON pp (b);
         ALTER TABLE ppi RENAME COLUMN b TO bz;
         CREATE TABLE c1 (a int, b int);
         ALTER TABLE pp ATTACH PARTITION c1 FOR VALUES FROM (1) TO (2);
         ALTER TABLE c1_b_idx RENAME COLUMN b TO q;
         CREATE TABLE t (a int, b int);
         CREATE INDEX ti ON t (b);",
    )]);
    // In a later transaction the catalog's names are read again.
    db.apply_sql(
        "CREATE TABLE c2 (a int, b int);
         ALTER TABLE pp ATTACH PARTITION c2 FOR VALUES FROM (2) TO (3);
         ALTER TABLE c2_bz_idx RENAME COLUMN bz TO q;",
    )
    .unwrap();
    db.apply_sql(
        "TRUNCATE t;
         ALTER TABLE ti RENAME COLUMN b TO bz;
         CREATE TABLE t2 (LIKE t INCLUDING INDEXES);
         ALTER TABLE t2_b_idx RENAME COLUMN b TO q;",
    )
    .unwrap();
    db.apply_sql(
        "REINDEX INDEX ti;
         ALTER TABLE ti RENAME COLUMN bz TO bq;
         CREATE TABLE t3 (LIKE t INCLUDING INDEXES);
         ALTER TABLE t3_bz_idx RENAME COLUMN bz TO q;",
    )
    .unwrap();
    db.apply_sql(
        "ALTER TABLE t ALTER COLUMN b TYPE bigint;
         ALTER TABLE ti RENAME COLUMN b TO bw;
         CREATE TABLE t4 (LIKE t INCLUDING INDEXES);
         ALTER TABLE t4_b_idx RENAME COLUMN b TO q;",
    )
    .unwrap();
    db.apply_sql(
        "ALTER TABLE ti RENAME COLUMN bw TO bv;
         CREATE TABLE t5 (LIKE t INCLUDING INDEXES);
         ALTER TABLE t5_bv_idx RENAME COLUMN bv TO q;",
    )
    .unwrap();
}

/// Whether `op`, run in the transaction that then renames index `ti`'s
/// column `b` and copies the table, leaves the copy with the old name
/// (`ti`'s relcache entry kept, its relfilenode new) or the new one.
fn assert_keeps_relcache(op: &str, kept: bool) {
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int, c int, v varchar(10), n numeric(10, 2), x text,
                         ts timestamp, i interval(3));
         CREATE INDEX ti ON t (b);
         CREATE DOMAIN dcheck AS int CHECK (VALUE > 0);",
    )]);
    let name = if kept { "b" } else { "bz" };
    let sql = format!(
        "{op};
         ALTER TABLE ti RENAME COLUMN b TO bz;
         CREATE TABLE q2 (LIKE t INCLUDING INDEXES);
         ALTER TABLE q2_{name}_idx RENAME COLUMN {name} TO q;"
    );
    if let Err(err) = db.apply_sql(&sql) {
        panic!("{op}\n  rejected: {err}");
    }
}

#[test]
fn every_rewrite_of_a_table_keeps_its_indexes_relcache_entries() {
    // finish_heap_swap rebuilds every index of a rewritten table, each in a
    // new relfilenode, whose relcache entry then stays for the rest of the
    // transaction: TRUNCATE, CLUSTER, REINDEX, and ALTER TABLE's rewrites —
    // a column type change that isn't a relabeling (whatever column), a
    // volatile or constrained-domain default, a stored generated column,
    // SET LOGGED / UNLOGGED.
    for op in [
        "TRUNCATE t",
        "REINDEX TABLE t",
        "CLUSTER t USING ti",
        "ALTER TABLE t ALTER COLUMN c TYPE bigint",
        "ALTER TABLE t ALTER COLUMN b TYPE bigint",
        "ALTER TABLE t ALTER COLUMN c TYPE int USING c + 0",
        "ALTER TABLE t ALTER COLUMN v TYPE varchar(5)",
        "ALTER TABLE t ALTER COLUMN x TYPE varchar(10)",
        "ALTER TABLE t ALTER COLUMN n TYPE numeric(12, 3)",
        "ALTER TABLE t ALTER COLUMN i TYPE interval day",
        "ALTER TABLE t ALTER COLUMN c TYPE dcheck",
        "ALTER TABLE t ADD COLUMN d float8 DEFAULT random()",
        "ALTER TABLE t ADD COLUMN d int GENERATED ALWAYS AS (a + 1) STORED",
        "ALTER TABLE t ADD COLUMN d dcheck",
        "ALTER TABLE t ADD COLUMN d serial",
        "ALTER TABLE t SET UNLOGGED",
    ] {
        assert_keeps_relcache(op, true);
    }
    // A relabeling, a missing value, a catalog-only change: no rewrite.
    for op in [
        "SELECT 1",
        "ALTER TABLE t ALTER COLUMN v TYPE varchar(20)",
        "ALTER TABLE t ALTER COLUMN v TYPE text",
        "ALTER TABLE t ALTER COLUMN v TYPE text USING v::text",
        "ALTER TABLE t ALTER COLUMN x TYPE varchar",
        "ALTER TABLE t ALTER COLUMN n TYPE numeric(12, 2)",
        "ALTER TABLE t ALTER COLUMN n TYPE numeric",
        "ALTER TABLE t ALTER COLUMN i TYPE interval(6)",
        "ALTER TABLE t ALTER COLUMN c TYPE int",
        "ALTER TABLE t ALTER COLUMN c TYPE oid",
        "ALTER TABLE t ADD COLUMN d int DEFAULT 5",
        "ALTER TABLE t ADD COLUMN d int GENERATED ALWAYS AS (a + 1) VIRTUAL",
        "ALTER TABLE t ALTER COLUMN b SET STORAGE PLAIN",
        "ALTER TABLE t ADD CHECK (a > 0)",
        "ALTER TABLE t SET LOGGED",
    ] {
        assert_keeps_relcache(op, false);
    }
}

#[test]
fn a_partitioned_index_has_no_relfilenode_to_renew() {
    // TRUNCATE of a partitioned table renews its partitions' indexes, not
    // its own (they have no storage); REFRESH MATERIALIZED VIEW renews a
    // materialized view's.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int, b int) PARTITION BY RANGE (a);
         CREATE INDEX pi ON p (b);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (1) TO (2);
         CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a, 2 AS b;
         CREATE INDEX mvi ON mv (b);",
    )]);
    db.apply_sql(
        "TRUNCATE p;
         ALTER TABLE pi RENAME COLUMN b TO bz;
         CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (2) TO (3);
         ALTER TABLE p2_bz_idx RENAME COLUMN bz TO q;",
    )
    .unwrap();
    db.apply_sql(
        "TRUNCATE p;
         ALTER TABLE p1_b_idx RENAME COLUMN bz TO bq;
         CREATE TABLE l (LIKE p1 INCLUDING INDEXES);
         ALTER TABLE l_bz_idx RENAME COLUMN bz TO q;",
    )
    .unwrap();
    db.apply_sql(
        "REFRESH MATERIALIZED VIEW mv;
         ALTER TABLE mvi RENAME COLUMN b TO bz;
         CREATE TABLE l2 (LIKE mv INCLUDING INDEXES);
         ALTER TABLE l2_b_idx RENAME COLUMN b TO q;",
    )
    .unwrap();
}

#[test]
fn expression_index_columns_are_named_as_figure_index_colname_names_them() {
    // FigureIndexColname: a function's name, a cast's argument's, `case`,
    // `coalesce`, ...; `expr` for an operator — deduplicated with a
    // counter. ChooseIndexName names the index after them, and a clone
    // copies them.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE s (a text, b int);
         CREATE INDEX e ON s (lower(a::text));
         CREATE INDEX ON s (abs(b), lower(a), (a::varchar), (a || 'x'), (b + 1), lower(a),
                            (CASE WHEN b > 0 THEN 1 END), coalesce(b, 0));
         CREATE TABLE pe (a text, b int) PARTITION BY RANGE (b);
         CREATE INDEX pei ON pe (abs(b), lower(a), (a::text), (a || 'x'));",
    )]);
    db.apply_sql(
        "ALTER TABLE e RENAME COLUMN lower TO lw;
         ALTER TABLE s_abs_lower_a_expr_expr1_lower1_case_coalesce_idx RENAME COLUMN expr1 TO e1;
         ALTER TABLE s_abs_lower_a_expr_expr1_lower1_case_coalesce_idx RENAME COLUMN lower1 TO l1;
         ALTER TABLE s_abs_lower_a_expr_expr1_lower1_case_coalesce_idx RENAME COLUMN \"case\" TO c;",
    )
    .unwrap();
    db.apply_sql(
        "CREATE TABLE pe1 PARTITION OF pe FOR VALUES FROM (1) TO (2);
         ALTER TABLE pe1_abs_lower_a_expr_idx RENAME COLUMN expr TO x;
         ALTER TABLE pei RENAME COLUMN lower TO lw;
         CREATE TABLE l (LIKE pe INCLUDING INDEXES);
         ALTER TABLE l_abs_lw_a_expr_idx RENAME COLUMN lw TO q;",
    )
    .unwrap();
}
