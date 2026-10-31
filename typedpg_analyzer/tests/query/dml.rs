//! INSERT / UPDATE / DELETE with RETURNING, FROM, WHERE, and their
//! interaction with JOINs, subqueries, and CTEs.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');
         CREATE DOMAIN user_prefs AS JSONB;
         CREATE TABLE users (
            id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name        TEXT NOT NULL,
            email       TEXT NOT NULL UNIQUE,
            age         INT,
            role        user_role NOT NULL DEFAULT 'viewer',
            preferences user_prefs,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
         );
         CREATE TABLE posts (
            id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            user_id      BIGINT NOT NULL REFERENCES users(id),
            title        TEXT NOT NULL,
            body         TEXT,
            published_at TIMESTAMPTZ
         );
         CREATE TABLE comments (
            id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            post_id     BIGINT NOT NULL REFERENCES posts(id),
            author_name TEXT NOT NULL,
            content     TEXT NOT NULL,
            rating      INT
         );",
    )
    .unwrap();
    db
}

// ── Unknown column in DML — must match PostgreSQL's error ────────────────────
//
// PG rejects `INSERT INTO t (ghost) VALUES (...)` and `UPDATE t SET ghost = ...`
// with `column "ghost" of relation "t" does not exist`. The analyzer must do
// the same — treating the column as unknown-typed and silently picking `text`
// would mask a real bug in the caller's SQL.

#[test]
fn insert_into_nonexistent_column_errors() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("INSERT INTO users (nonexistent) VALUES ($p1)"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"nonexistent\" of relation \"users\" does not exist\n",
            "  ╭────\n",
            "1 │ INSERT INTO users (nonexistent) VALUES ($p1)\n",
            "  ·                    ─────┬─────\n",
            "  ·                         ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn update_set_nonexistent_column_errors() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("UPDATE users SET nonexistent = $p1 WHERE id = $p2"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"nonexistent\" of relation \"users\" does not exist\n",
            "  ╭────\n",
            "1 │ UPDATE users SET nonexistent = $p1 WHERE id = $p2\n",
            "  ·                  ─────┬─────\n",
            "  ·                       ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

// ── INSERT … RETURNING ───────────────────────────────────────────────────────

#[test]
fn insert_returning() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id, name, age")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn insert_all_columns() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) RETURNING *")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("name", text()),
            c("email", text()),
            cn("age", int4()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
            cn("preferences", domain("public", "user_prefs", jsonb())),
            c("created_at", timestamptz()),
        ],
    );
    assert_params(&s, vec![p(text()), p(text()), pn(int4())]);
}

#[test]
fn insert_multiple_rows() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email) VALUES ($p1, $p2), ($p3, $p4) RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text()), p(text()), p(text())]);
}

/// transformUpdateStmt / transformDeleteStmt keep the target LATERAL-only
/// and not LATERAL-ok while the FROM / USING list is transformed: a
/// LATERAL item or FROM function reaching it is 42P10, a plain subquery or
/// a JOIN's ON clause the generic `invalid reference` (42P01). MERGE's
/// target is not in the namespace at all while the source is transformed.
#[test]
fn from_list_cannot_reference_the_dml_target() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL, w text);
         CREATE TABLE s (id int PRIMARY KEY, v int NOT NULL, x text);",
    )
    .unwrap();
    for sql in [
        "UPDATE t SET v = q.v FROM LATERAL (SELECT t.v) q",
        "UPDATE t SET v = s.v FROM s JOIN LATERAL (SELECT t.v) q ON true",
        "UPDATE t SET v = 1 FROM LATERAL (SELECT w) q",
        "UPDATE t SET v = 1 FROM generate_series(1, t.v) g",
        "DELETE FROM t USING LATERAL (SELECT t.v) q",
        "DELETE FROM t USING s, LATERAL (SELECT t.v) q",
        "DELETE FROM t USING generate_series(1, v) g",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::InvalidColumnReference(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("invalid reference to FROM-clause entry for table \"t\""),
            "{sql}: {err}"
        );
    }
    for sql in [
        "UPDATE t SET v = 1 FROM (SELECT t.v) q",
        "UPDATE t SET v = 1 FROM s JOIN s s2 ON t.id = s2.id",
        "MERGE INTO t USING (s JOIN LATERAL (SELECT t.v) q ON true) ON t.id = s.id \
         WHEN MATCHED THEN DELETE",
        "MERGE INTO t USING generate_series(1, t.v) g ON true WHEN MATCHED THEN DELETE",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::UndefinedTable(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("invalid reference to FROM-clause entry for table \"t\""),
            "{sql}: {err}"
        );
    }
    // WHERE and SET see the target normally.
    db.analyze("UPDATE t SET v = q.v FROM LATERAL (SELECT s.v FROM s) q WHERE t.id = q.v")
        .unwrap();
}

/// transformInsertStmt checks each later VALUES row against the first
/// row's length before the per-row arity rule, so a short second row is a
/// VALUES error, not an INSERT arity one.
#[test]
fn insert_values_rows_must_share_a_length() {
    let db = setup();
    let err = db
        .analyze("INSERT INTO users (name, email) VALUES ('a', 'b'), ('c')")
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::SyntaxError(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("VALUES lists must all be the same length"),
        "{err}"
    );
    // A short first row still trips the arity rule.
    let err = db
        .analyze("INSERT INTO users (name, email) VALUES ('a'), ('c', 'd')")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("INSERT has more target columns than expressions"),
        "{err}"
    );
}

#[test]
fn insert_into_posts() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO posts (user_id, title, body) VALUES ($p1, $p2, $p3) RETURNING id, title",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("title", text())]);
    assert_params(&s, vec![p(int8()), p(text()), pn(text())]);
}

#[test]
fn insert_into_comments() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO comments (post_id, author_name, content, rating) \
             VALUES ($p1, $p2, $p3, $p4) RETURNING *",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("post_id", int8()),
            c("author_name", text()),
            c("content", text()),
            cn("rating", int4()),
        ],
    );
    assert_params(&s, vec![p(int8()), p(text()), p(text()), pn(int4())]);
}

// ── UPDATE … RETURNING ───────────────────────────────────────────────────────

#[test]
fn update_returning() {
    let db = setup();
    let s = db
        .analyze("UPDATE users SET age = $p1 WHERE id = $p2 RETURNING id, name, age")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
    assert_params(&s, vec![pn(int4()), p(int8())]);
}

#[test]
fn update_multiple_columns() {
    let db = setup();
    let s = db
        .analyze("UPDATE users SET name = $p1, email = $p2, age = $p3 WHERE id = $p4 RETURNING *")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("name", text()),
            c("email", text()),
            cn("age", int4()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
            cn("preferences", domain("public", "user_prefs", jsonb())),
            c("created_at", timestamptz()),
        ],
    );
    assert_params(&s, vec![p(text()), p(text()), pn(int4()), p(int8())]);
}

#[test]
fn update_with_from() {
    let db = setup();
    let s = db
        .analyze(
            "UPDATE posts SET title = $p1 \
             FROM users u WHERE posts.user_id = u.id AND u.name = $p2 \
             RETURNING posts.id, posts.title",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("title", text())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn update_set_coalesce_param_is_nullable() {
    // `SET col = COALESCE($p, col)` is the canonical "patch this field
    // only if the caller supplied a value" pattern. The param has to be
    // typed nullable — otherwise the COALESCE is pointless and the caller
    // would be forced to wrap every value in `Some(...)`.
    let db = setup();
    let s = db
        .analyze("UPDATE posts SET title = COALESCE($p1, title) WHERE id = $p2 RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![pn(text()), p(int8())]);
}

#[test]
fn update_set_coalesce_param_on_not_null_column_still_nullable() {
    // Even when the target column is NOT NULL, the param sitting inside
    // COALESCE is still nullable — the COALESCE itself is what guarantees
    // the assignment never receives NULL.
    let db = setup();
    let s = db
        .analyze("UPDATE users SET name = COALESCE($p1, name) WHERE id = $p2 RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![pn(text()), p(int8())]);
}

// ── DELETE … RETURNING ───────────────────────────────────────────────────────

#[test]
fn delete_returning() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM users WHERE id = $p1 RETURNING id, name, age")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn delete_returning_star() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM comments WHERE post_id = $p1 RETURNING *")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("post_id", int8()),
            c("author_name", text()),
            c("content", text()),
            cn("rating", int4()),
        ],
    );
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn delete_returning_subset() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM posts WHERE user_id = $p1 RETURNING id, title")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("title", text())]);
    assert_params(&s, vec![p(int8())]);
}

// ── INSERT … SELECT ──────────────────────────────────────────────────────────

#[test]
fn insert_select() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO comments (post_id, author_name, content) \
             SELECT p.id, $p1, $p2 FROM posts p WHERE p.user_id = $p3 \
             RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text()), p(int8())]);
}

#[test]
fn complex_insert_select_with_join() {
    let db = setup();
    // INSERT ... SELECT from a JOIN — params come from WHERE.
    let sql = "INSERT INTO comments (post_id, author_name, content) \
               SELECT p.id, $p1, $p2 \
               FROM posts p \
               WHERE p.user_id = $p3 \
               RETURNING id, post_id, author_name";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            c("post_id", int8()),
            c("author_name", text()),
        ],
    );
}

// ── Static rejection of NOT NULL violations and arity mismatches ────────────

#[test]
fn update_set_not_null_column_to_null_rejected() {
    // PG only catches NULL-into-NOT-NULL at runtime (`null value in
    // column "name" violates not-null constraint`); the analyzer catches
    // it statically. PG sanity's `prepare` doesn't reach runtime evaluation,
    // so opt out of the mirror — analyzer behavior is the load-bearing
    // assertion here.
    // The pg_sanity execute fallback fires INSERTs with all-NULL params,
    // but UPDATE on a freshly-created scratch table affects 0 rows and
    // doesn't reach the row-level NOT NULL check — keep the skip and rely
    // on the analyzer's stricter compile-time guard here.
    let mut db = setup();
    db.skip_pg_sanity();
    assert_analyze_err!(
        db.analyze("UPDATE users SET name = NULL WHERE id = $p1 RETURNING id"),
        AnalyzeError::Invalid(_),
        "null value in column \"name\" of relation \"users\" violates not-null constraint (cannot assign NULL to NOT NULL column `users.name`)",
    );
}

#[test]
fn insert_null_into_not_null_column_rejected() {
    // INSERT with a literal NULL hits PG's row-level constraint at execute
    // time; the pg_sanity fallback observes the same wording the analyzer
    // emits, so no skip needed here.
    let db = setup();
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email) VALUES (NULL, $p1)"),
        AnalyzeError::Invalid(_),
        "null value in column \"name\" of relation \"users\" violates not-null constraint (cannot insert NULL into NOT NULL column `users.name`)",
    );
}

#[test]
fn insert_null_reports_the_first_null_column_in_table_order() {
    // PG checks a row's NOT NULL columns in attnum order once the row is
    // built: an omitted column with no default (posts.user_id) comes
    // before the literal NULL (posts.title), and the column list's order
    // doesn't matter.
    let db = setup();
    assert_analyze_err!(
        db.analyze("INSERT INTO posts (title) VALUES (NULL)"),
        AnalyzeError::Invalid(_),
        "null value in column \"user_id\" of relation \"posts\" violates not-null constraint (cannot insert NULL into NOT NULL column `posts.user_id`)",
    );
    assert_analyze_err!(
        db.analyze("INSERT INTO users (email, name) VALUES (NULL, NULL)"),
        AnalyzeError::Invalid(_),
        "null value in column \"name\" of relation \"users\" violates not-null constraint (cannot insert NULL into NOT NULL column `users.name`)",
    );
    // An explicit DEFAULT that is NULL counts like an omitted column.
    assert_analyze_err!(
        db.analyze("INSERT INTO posts (title, user_id) VALUES (NULL, DEFAULT)"),
        AnalyzeError::Invalid(_),
        "null value in column \"user_id\" of relation \"posts\" violates not-null constraint (cannot insert NULL into NOT NULL column `posts.user_id`)",
    );
    // The first failing row is the one reported.
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email) VALUES ('a', 'b'), ('c', NULL), (NULL, 'd')"),
        AnalyzeError::Invalid(_),
        "null value in column \"email\" of relation \"users\" violates not-null constraint (cannot insert NULL into NOT NULL column `users.email`)",
    );
    // An error PG raises while parsing comes first, wherever it is.
    assert_err_prefix!(
        db.analyze("INSERT INTO users (name, email, age) VALUES (NULL, 'b', 'x')"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"x\""
    );
}

/// `DEFAULT NULL` on a NOT NULL column is no default: AddRelationNewConstraints
/// stores none, so every INSERT leaving the column to it fails. On a domain
/// column the cooked default is a CoerceToDomain over NULL, which PG keeps
/// and which overrides the domain's own default: still NULL.
#[test]
fn insert_leaving_a_not_null_column_to_default_null_is_rejected() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE z2 (a int NOT NULL DEFAULT NULL, b int);
         CREATE TABLE z (a int NOT NULL DEFAULT 1, b int);
         ALTER TABLE z ALTER a SET DEFAULT NULL;
         CREATE DOMAIN dm AS int DEFAULT 5;
         CREATE TABLE z3 (a dm NOT NULL DEFAULT NULL, b int);
         CREATE TABLE z3c (a dm NOT NULL DEFAULT NULL::int, b int);
         CREATE DOMAIN dnn AS int NOT NULL DEFAULT 5;
         CREATE TABLE z4 (a dnn DEFAULT NULL, b int);
         CREATE TABLE z5 (b int);
         ALTER TABLE z5 ADD COLUMN a int NOT NULL DEFAULT NULL;
         CREATE TABLE z6 (a varchar(5) NOT NULL DEFAULT NULL, b int);
         CREATE TABLE z7 (a int NOT NULL DEFAULT NULL::int, b int);
         CREATE TABLE p (a int NOT NULL DEFAULT 7);
         CREATE TABLE c (a int DEFAULT NULL) INHERITS (p);",
    )
    .unwrap();
    let not_null = |rel: &str| {
        format!(r#"null value in column "a" of relation "{rel}" violates not-null constraint"#)
    };
    for (sql, rel) in [
        ("INSERT INTO z2 (b) VALUES (1)", "z2"),
        ("INSERT INTO z2 DEFAULT VALUES", "z2"),
        ("INSERT INTO z2 (a, b) VALUES (DEFAULT, 1)", "z2"),
        ("INSERT INTO z (b) VALUES (1)", "z"),
        ("INSERT INTO z3 (b) VALUES (1)", "z3"),
        ("INSERT INTO z3 (a, b) VALUES (DEFAULT, 1)", "z3"),
        ("INSERT INTO z3c (b) VALUES (1)", "z3c"),
        ("INSERT INTO z5 (b) VALUES (1)", "z5"),
        ("INSERT INTO z6 (b) VALUES (1)", "z6"),
        ("INSERT INTO z7 (b) VALUES (1)", "z7"),
        ("INSERT INTO c DEFAULT VALUES", "c"),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::Invalid(_), &not_null(rel));
    }
    // A NOT NULL domain fails as the row is built, the column's own
    // DEFAULT NULL overriding the domain's DEFAULT 5.
    assert_err_prefix!(
        db.analyze("INSERT INTO z4 (b) VALUES (1)"),
        AnalyzeError::Invalid(_),
        "domain dnn does not allow null values"
    );
}

/// Neighbours of the above PG accepts: a BEFORE ROW trigger may fill the
/// column, a view's `DEFAULT NULL` is no default (the base table's
/// applies), and a NULL cast to another type goes through a cast function
/// the analyzer doesn't assume keeps it NULL.
#[test]
fn insert_leaving_a_column_to_default_null_is_accepted_where_it_may_succeed() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE z8 (a int NOT NULL DEFAULT NULL, b int);
         CREATE FUNCTION fill() RETURNS trigger LANGUAGE plpgsql
             AS $$BEGIN NEW.a := 1; RETURN NEW; END$$;
         CREATE TRIGGER t BEFORE INSERT ON z8 FOR EACH ROW EXECUTE FUNCTION fill();
         CREATE TABLE z (a int NOT NULL DEFAULT 3, b int);
         CREATE VIEW v AS SELECT * FROM z;
         ALTER VIEW v ALTER a SET DEFAULT NULL;
         CREATE TABLE n (a int DEFAULT NULL, b int);
         CREATE DOMAIN dm AS int DEFAULT 5;
         CREATE TABLE nd (a dm DEFAULT NULL, b int);",
    )
    .unwrap();
    for sql in [
        "INSERT INTO z8 (b) VALUES (1)",
        "INSERT INTO v (b) VALUES (1) RETURNING a",
        "INSERT INTO n (b) VALUES (1)",
        "INSERT INTO nd (b) VALUES (1)",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// A row inserted into a partitioned table is routed to its partition
/// before ExecConstraints runs, so PG names the partition the row lands
/// in — through every level of sub-partitioning.
#[test]
fn not_null_violation_names_the_partition_the_row_goes_to() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE errtst (a text, b text NOT NULL, c int) PARTITION BY LIST (a);
         CREATE TABLE errtst_part_1 PARTITION OF errtst FOR VALUES IN ('aaa');
         CREATE TABLE errtst_part_2 PARTITION OF errtst FOR VALUES IN ('bbb', NULL)
             PARTITION BY RANGE (c);
         CREATE TABLE errtst_part_2_lo PARTITION OF errtst_part_2 FOR VALUES FROM (0) TO (10);
         CREATE TABLE errtst_part_2_hi PARTITION OF errtst_part_2
             FOR VALUES FROM (10) TO (MAXVALUE);
         CREATE TABLE errtst_default PARTITION OF errtst DEFAULT;",
    )
    .unwrap();
    let not_null = |relation: &str| {
        format!(r#"null value in column "b" of relation "{relation}" violates not-null constraint"#)
    };
    for (sql, relation) in [
        (
            "INSERT INTO errtst (a, b) VALUES ('aaa', NULL)",
            "errtst_part_1",
        ),
        (
            "INSERT INTO errtst VALUES ('bbb', NULL, 3)",
            "errtst_part_2_lo",
        ),
        (
            "INSERT INTO errtst (c, b) VALUES (12, NULL)",
            "errtst_part_2_hi",
        ),
        (
            "INSERT INTO errtst (a, b, c) VALUES ('zzz', NULL, 1)",
            "errtst_default",
        ),
        // The second row is the first to fail.
        (
            "INSERT INTO errtst (a, b) VALUES ('aaa', 'x'), ('aaa', NULL)",
            "errtst_part_1",
        ),
        // A sub-partitioned table is a partition too.
        (
            "INSERT INTO errtst_part_2 VALUES ('bbb', NULL, 50)",
            "errtst_part_2_hi",
        ),
        // MERGE inserts through the same routing.
        (
            "MERGE INTO errtst t USING (SELECT 1 k) s ON false \
             WHEN NOT MATCHED THEN INSERT (a, b) VALUES ('aaa', NULL)",
            "errtst_part_1",
        ),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            &not_null(relation)
        );
    }
}

/// Where the analyzer can't tell which partition PG routes the row to, the
/// NOT NULL violation names the target table rather than guess a partition.
/// PG names the leaf it routes the row to at run time (or, for a row no
/// partition takes, reports that first), so these diverge from it by
/// design and opt out of the mirror.
#[test]
fn not_null_violation_names_the_target_when_the_partition_is_unknown() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE errtst (a text, b text NOT NULL, c int) PARTITION BY LIST (a);
         CREATE TABLE errtst_part_1 PARTITION OF errtst FOR VALUES IN ('aaa');
         CREATE TABLE errtst_part_2 PARTITION OF errtst FOR VALUES IN ('bbb', NULL)
             PARTITION BY RANGE (c);
         CREATE TABLE errtst_part_2_lo PARTITION OF errtst_part_2 FOR VALUES FROM (0) TO (10);
         CREATE TABLE errtst_default PARTITION OF errtst DEFAULT;
         CREATE TABLE hashed (a int, b text NOT NULL) PARTITION BY HASH (a);
         CREATE TABLE hashed_0 PARTITION OF hashed FOR VALUES WITH (MODULUS 2, REMAINDER 0);
         CREATE TABLE hashed_1 PARTITION OF hashed FOR VALUES WITH (MODULUS 2, REMAINDER 1);
         CREATE COLLATION user_c FROM \"C\";
         CREATE TABLE collated (a text COLLATE user_c, b text NOT NULL) PARTITION BY LIST (a);
         CREATE TABLE collated_aaa PARTITION OF collated FOR VALUES IN ('aaa');
         CREATE TABLE collated_default PARTITION OF collated DEFAULT;",
    )
    .unwrap();
    db.skip_pg_sanity();
    let not_null = |relation: &str| {
        format!(r#"null value in column "b" of relation "{relation}" violates not-null constraint"#)
    };
    for (sql, relation) in [
        // A parameter key: the partition depends on its value.
        ("INSERT INTO errtst (a, b) VALUES ($1, NULL)", "errtst"),
        // A key that is not a literal (PG routes it to errtst_part_1).
        (
            "INSERT INTO errtst (a, b) VALUES ('a' || 'aa', NULL)",
            "errtst",
        ),
        // HASH partitioning (PG routes 1 to hashed_0).
        ("INSERT INTO hashed VALUES (1, NULL)", "hashed"),
        // A text key under a user's collation (PG routes it to collated_aaa).
        ("INSERT INTO collated VALUES ('aaa', NULL)", "collated"),
        (
            "MERGE INTO errtst t USING (SELECT 1 k) s ON false \
             WHEN NOT MATCHED THEN INSERT (a, b) VALUES ($1, NULL)",
            "errtst",
        ),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            &not_null(relation)
        );
    }
}

/// A VALUES row no partition takes fails every execution in
/// ExecFindPartition (`no partition of relation "…" found for row`,
/// 23514): rejected where the analyzer can tell, naming the partitioned
/// table — the root or a sub-partition — whose partitions all refuse it.
/// Routing comes after the row is built and before its NOT NULL checks.
#[test]
fn insert_row_no_partition_takes_is_rejected() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE errtst (a text, b text NOT NULL, c int) PARTITION BY LIST (a);
         CREATE TABLE errtst_part_1 PARTITION OF errtst FOR VALUES IN ('aaa');
         CREATE TABLE errtst_part_2 PARTITION OF errtst FOR VALUES IN ('bbb', NULL)
             PARTITION BY RANGE (c);
         CREATE TABLE errtst_part_2_lo PARTITION OF errtst_part_2 FOR VALUES FROM (0) TO (10);
         CREATE TABLE errtst_default PARTITION OF errtst DEFAULT;
         CREATE DOMAIN nn_text AS text NOT NULL;
         CREATE TABLE nodef (a int, b text NOT NULL, d nn_text DEFAULT 'd')
             PARTITION BY LIST (a);
         CREATE TABLE nodef_1 PARTITION OF nodef FOR VALUES IN (1);
         CREATE TABLE rng (a int, b int) PARTITION BY RANGE (a, b);
         CREATE TABLE rng_1 PARTITION OF rng FOR VALUES FROM (0, 0) TO (10, 10);
         CREATE TABLE dt (a date) PARTITION BY RANGE (a);
         CREATE TABLE dt_1 PARTITION OF dt FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');",
    )
    .unwrap();
    for (sql, relation) in [
        // The sub-partition errtst_part_2 takes 'bbb', but none of its
        // own partitions takes c = -5.
        (
            "INSERT INTO errtst VALUES ('bbb', 'x', -5)",
            "errtst_part_2",
        ),
        // A NULL range key goes only to a default partition.
        ("INSERT INTO errtst (b) VALUES ('x')", "errtst_part_2"),
        ("INSERT INTO nodef VALUES (2, 'x')", "nodef"),
        ("INSERT INTO nodef VALUES (NULL, 'x')", "nodef"),
        ("INSERT INTO nodef (b) VALUES ('x')", "nodef"),
        ("INSERT INTO nodef DEFAULT VALUES", "nodef"),
        // Before the row's NULL into a NOT NULL column.
        ("INSERT INTO nodef VALUES (2, NULL)", "nodef"),
        // The second row is the first to fail.
        ("INSERT INTO nodef VALUES (1, 'x'), (2, 'x')", "nodef"),
        // ON CONFLICT doesn't save it: the row is routed first.
        (
            "INSERT INTO nodef VALUES (2, 'x') ON CONFLICT DO NOTHING",
            "nodef",
        ),
        // Multi-column range bounds compare lexicographically.
        ("INSERT INTO rng VALUES (10, 10)", "rng"),
        ("INSERT INTO rng VALUES (-1, 5)", "rng"),
        ("INSERT INTO rng VALUES (NULL, 1)", "rng"),
        // Constant keys as PG folds and coerces them.
        ("INSERT INTO nodef VALUES (1 + 1, 'x')", "nodef"),
        ("INSERT INTO nodef VALUES (2.5, 'x')", "nodef"),
        ("INSERT INTO dt VALUES ('2025-01-01')", "dt"),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::CheckViolation(_),
            &format!(r#"no partition of relation "{relation}" found for row"#)
        );
    }
    // The row is built first: a NULL into a NOT NULL domain fails before
    // routing.
    assert_err_prefix!(
        db.analyze("INSERT INTO nodef VALUES (2, 'x', NULL)"),
        AnalyzeError::Invalid(_),
        r#"domain nn_text does not allow null values"#
    );
    // A row a partition takes.
    for sql in [
        "INSERT INTO nodef VALUES (1, 'x')",
        "INSERT INTO rng VALUES (10, 9)",
        "INSERT INTO rng VALUES (0, 0)",
        "INSERT INTO errtst VALUES ('bbb', 'x', 5)",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// Where the analyzer can't tell that no partition takes the row — or the
/// statement doesn't insert it every time — the INSERT stays accepted.
#[test]
fn insert_row_routing_uncertain_is_accepted() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE nodef (a int, b text) PARTITION BY LIST (a);
         CREATE TABLE nodef_1 PARTITION OF nodef FOR VALUES IN (1);
         CREATE TABLE keydef (a int DEFAULT 2, b text) PARTITION BY LIST (a);
         CREATE TABLE keydef_1 PARTITION OF keydef FOR VALUES IN (1);
         CREATE TABLE hashed (a int) PARTITION BY HASH (a);
         CREATE TABLE hashed_0 PARTITION OF hashed FOR VALUES WITH (MODULUS 2, REMAINDER 0);
         CREATE COLLATION user_c FROM \"C\";
         CREATE TABLE collated (a text COLLATE user_c) PARTITION BY LIST (a);
         CREATE TABLE collated_aaa PARTITION OF collated FOR VALUES IN ('aaa');
         CREATE TABLE exprkey (a int) PARTITION BY LIST ((a + 1));
         CREATE TABLE exprkey_1 PARTITION OF exprkey FOR VALUES IN (1);
         CREATE TABLE sk (a int2) PARTITION BY LIST (a);
         CREATE TABLE sk_1 PARTITION OF sk FOR VALUES IN (1);
         CREATE TABLE bigk (a int8) PARTITION BY LIST (a);
         CREATE TABLE bigk_1 PARTITION OF bigk FOR VALUES IN (1);
         CREATE DOMAIN pos AS int CHECK (VALUE > 0);
         CREATE TABLE dk (a pos) PARTITION BY LIST (a);
         CREATE TABLE dk_1 PARTITION OF dk FOR VALUES IN (1);
         CREATE TABLE tz (a timestamptz) PARTITION BY RANGE (a);
         CREATE TABLE tz_1 PARTITION OF tz FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
         CREATE TABLE ruled (a int) PARTITION BY LIST (a);
         CREATE TABLE ruled_1 PARTITION OF ruled FOR VALUES IN (1);
         CREATE TABLE sink (a int);
         CREATE RULE r AS ON INSERT TO ruled DO INSTEAD INSERT INTO sink VALUES (NEW.a);",
    )
    .unwrap();
    for sql in [
        // A parameter key: the partition depends on its value.
        "INSERT INTO nodef VALUES ($1, 'x')",
        // A key that is not a constant.
        "INSERT INTO nodef VALUES (length('ab'), 'x')",
        // A domain's CHECK may reject the key first.
        "INSERT INTO dk VALUES (-5)",
        // A timestamptz literal reads in the session's time zone.
        "INSERT INTO tz VALUES ('2025-02-01')",
        // A key left to a non-NULL default.
        "INSERT INTO keydef (b) VALUES ('x')",
        // INSERT … SELECT: the rows come from the query.
        "INSERT INTO nodef SELECT 2, 'x'",
        // HASH partitioning.
        "INSERT INTO hashed VALUES (1)",
        // A text key under a user's collation.
        "INSERT INTO collated VALUES ('zzz')",
        // An expression key.
        "INSERT INTO exprkey VALUES (5)",
        // A DO INSTEAD rule replaces the INSERT.
        "INSERT INTO ruled VALUES (2)",
        // MERGE inserts only for source rows matching nothing.
        "MERGE INTO nodef t USING (SELECT 1 k) s ON false \
         WHEN NOT MATCHED THEN INSERT VALUES (2, 'x')",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // A key out of its type's range: planning fails on that first, before
    // any row is routed.
    for (sql, msg) in [
        ("INSERT INTO sk VALUES (32767 + 1)", "smallint out of range"),
        (
            "INSERT INTO bigk VALUES (2147483647 + 1)",
            "integer out of range",
        ),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::NumericValueOutOfRange(_),
            msg
        );
    }
}

/// A VALUES with ORDER BY, LIMIT / OFFSET or its own WITH is analyzed as
/// a SELECT (transformInsertStmt's isGeneralSelect), but the rows it
/// yields are stored all the same: one no partition takes, or storing a
/// NULL into a NOT NULL column, fails every execution that stores it.
/// Without ORDER BY the rows come in order, those OFFSET / LIMIT keep;
/// with one, the rows failing must fail alike.
#[test]
fn insert_general_values_rows_are_checked() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE pt (a int, b int) PARTITION BY LIST (a);
         CREATE TABLE pt1 PARTITION OF pt (b NOT NULL) FOR VALUES IN (1);
         CREATE TABLE t (a int NOT NULL, b text NOT NULL);
         CREATE DOMAIN nn AS int NOT NULL;
         CREATE TABLE d (a nn);",
    )
    .unwrap();
    for (sql, message) in [
        (
            "INSERT INTO pt VALUES (3, 1) LIMIT 1",
            r#"no partition of relation "pt" found for row"#,
        ),
        (
            "INSERT INTO pt VALUES (1, NULL::int) LIMIT 1",
            r#"null value in column "b" of relation "pt1" violates not-null constraint"#,
        ),
        (
            "INSERT INTO pt WITH x AS (SELECT 1) VALUES (3, 1)",
            r#"no partition of relation "pt" found for row"#,
        ),
        // The rows LIMIT keeps, in order.
        (
            "INSERT INTO pt VALUES (1, 1), (3, 1) LIMIT 2",
            r#"no partition of relation "pt" found for row"#,
        ),
        (
            "INSERT INTO pt VALUES (3, 1), (1, 1) LIMIT 1",
            r#"no partition of relation "pt" found for row"#,
        ),
        (
            "INSERT INTO pt VALUES (1, 1), (3, 1) OFFSET 1",
            r#"no partition of relation "pt" found for row"#,
        ),
        (
            "INSERT INTO pt VALUES (1, NULL), (3, 1) LIMIT ALL",
            r#"null value in column "b" of relation "pt1" violates not-null constraint"#,
        ),
        // Sorted: every row stored, or every row failing alike.
        (
            "INSERT INTO pt VALUES (1, 1), (3, 1) ORDER BY 1",
            r#"no partition of relation "pt" found for row"#,
        ),
        (
            "INSERT INTO pt VALUES (3, 1), (4, 1) ORDER BY 1 LIMIT 1",
            r#"no partition of relation "pt" found for row"#,
        ),
        // A text NULL into a text column; an omitted column.
        (
            "INSERT INTO t VALUES (1, NULL) ORDER BY 1",
            r#"null value in column "b" of relation "t" violates not-null constraint"#,
        ),
        (
            "INSERT INTO t (b) VALUES ('x') LIMIT 1",
            r#"null value in column "a" of relation "t" violates not-null constraint"#,
        ),
        (
            "INSERT INTO d VALUES (NULL::int) LIMIT 1",
            "domain nn does not allow null values",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), _, message);
    }
    for sql in [
        // No row stored, or none of those failing.
        "INSERT INTO pt VALUES (3, 1) LIMIT 0",
        "INSERT INTO pt VALUES (3, 1), (1, 1) OFFSET 1",
        "INSERT INTO pt VALUES (1, 1), (3, 1) LIMIT 1",
        // Which rows are stored depends on the sort / the parameter.
        "INSERT INTO pt VALUES (3, 1), (1, 1) ORDER BY 1 LIMIT 1",
        "INSERT INTO pt VALUES (3, 1) LIMIT $1",
        // A NULL of a domain fails as the VALUES is evaluated: not the
        // column's check (and here, no row is).
        "INSERT INTO d VALUES (NULL::nn) LIMIT 0",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // Rows failing differently, in an order the sort decides: which
    // error PG reports is left to it.
    db.analyze("INSERT INTO pt VALUES (1, NULL::int), (3, 1) ORDER BY 1")
        .unwrap();
}

/// A NULL stored into a NOT NULL column fails every execution, wherever
/// the NOT NULL is declared and however the NULL is written — unless
/// something may replace the row or the statement.
#[test]
fn insert_null_into_not_null_forms() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE pt (a int, b int) PARTITION BY LIST (a);
         CREATE TABLE pt1 PARTITION OF pt (b NOT NULL) FOR VALUES IN (1);
         CREATE TABLE ptt (a int, b int) PARTITION BY LIST (a);
         CREATE TABLE ptt1 PARTITION OF ptt (b NOT NULL) FOR VALUES IN (1);
         CREATE FUNCTION fill() RETURNS trigger LANGUAGE plpgsql
             AS $$BEGIN NEW.b := 5; RETURN NEW; END$$;
         CREATE TRIGGER tr BEFORE INSERT ON ptt1 FOR EACH ROW EXECUTE FUNCTION fill();
         CREATE TABLE t (a int NOT NULL, b text NOT NULL, c numeric NOT NULL DEFAULT 0);
         CREATE TABLE sink (a int);
         CREATE TABLE r (a int NOT NULL, b int);
         CREATE RULE r_ins AS ON INSERT TO r DO INSTEAD INSERT INTO sink VALUES (NEW.b);
         CREATE RULE r_upd AS ON UPDATE TO r DO INSTEAD NOTHING;
         CREATE TABLE rc (a int NOT NULL, b int);
         CREATE RULE rc_ins AS ON INSERT TO rc WHERE NEW.b = 5 DO INSTEAD NOTHING;
         CREATE DOMAIN nn AS int NOT NULL;
         CREATE DOMAIN pos AS int CHECK (VALUE > 0);
         CREATE TABLE dd (a int NOT NULL, b int, c text);",
    )
    .unwrap();
    for (sql, message) in [
        // The partition's own NOT NULL, checked once the row lands there.
        (
            "INSERT INTO pt VALUES (1, NULL)",
            r#"null value in column "b" of relation "pt1" violates not-null constraint"#,
        ),
        (
            "INSERT INTO pt (a) VALUES (1)",
            r#"null value in column "b" of relation "pt1" violates not-null constraint"#,
        ),
        // A NULL cast to the column's type is NULL all the same.
        (
            "INSERT INTO pt VALUES (1, NULL::int)",
            r#"null value in column "b" of relation "pt1" violates not-null constraint"#,
        ),
        (
            "MERGE INTO pt t USING (SELECT 1 k) s ON false \
             WHEN NOT MATCHED THEN INSERT VALUES (1, NULL)",
            r#"null value in column "b" of relation "pt1" violates not-null constraint"#,
        ),
        (
            "INSERT INTO t VALUES (NULL::int, 'x')",
            r#"null value in column "a" of relation "t" violates not-null constraint"#,
        ),
        // A column left to no default, with no NULL given.
        (
            "INSERT INTO t (b) VALUES ('x')",
            r#"null value in column "a" of relation "t" violates not-null constraint"#,
        ),
        (
            "INSERT INTO t (a, b) VALUES (DEFAULT, 'x')",
            r#"null value in column "a" of relation "t" violates not-null constraint"#,
        ),
        (
            "INSERT INTO t DEFAULT VALUES",
            r#"null value in column "a" of relation "t" violates not-null constraint"#,
        ),
        // The second row is the first to fail.
        (
            "INSERT INTO t (a, b) VALUES (1, 'x'), (2, DEFAULT)",
            r#"null value in column "b" of relation "t" violates not-null constraint"#,
        ),
        // A NULL cast to a NOT NULL domain fails as the row is built,
        // before the columns' NOT NULL, whatever the column's type.
        (
            "INSERT INTO dd VALUES (NULL, NULL::nn)",
            "domain nn does not allow null values",
        ),
        (
            "INSERT INTO dd (a, c) VALUES (1, NULL::nn)",
            "domain nn does not allow null values",
        ),
        (
            "INSERT INTO dd VALUES (1, 1), (1, NULL::nn)",
            "domain nn does not allow null values",
        ),
        (
            "INSERT INTO dd VALUES (1, NULL::nn) LIMIT 1",
            "domain nn does not allow null values",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::Invalid(_), message);
    }
    for sql in [
        // A BEFORE ROW trigger of the partition may fill the column in.
        "INSERT INTO ptt VALUES (1, NULL)",
        // Which error PG reports depends on the parameter: the partition
        // the row lands in, or no partition.
        "INSERT INTO pt VALUES ($1, NULL)",
        // DO INSTEAD rules replace the statement, conditional ones
        // depending on the row.
        "INSERT INTO r VALUES (NULL, 1)",
        "INSERT INTO r (b) VALUES (1)",
        "UPDATE r SET a = NULL",
        "INSERT INTO rc VALUES (NULL, 5)",
        // A domain's CHECK lets a NULL through.
        "INSERT INTO dd (a, b) VALUES (1, NULL::pos)",
        // Filled in by the column's default.
        "INSERT INTO t (a, b) VALUES (1, 'x')",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn insert_values_row_wrong_arity_rejected() {
    let db = setup();
    // Explicit column list (name, email) expects 2 values per row; we
    // pass 3. PG: `INSERT has more expressions than target columns`.
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email) VALUES ($p1, $p2, $p3)"),
        AnalyzeError::SyntaxError(_),
        concat!(
            "INSERT has more expressions than target columns\n",
            "  ╭────\n",
            "1 │ INSERT INTO users (name, email) VALUES ($p1, $p2, $p3)\n",
            "  ·                                                   ─┬─\n",
            "  ·                                                    ╰─ no target column for this value\n",
            "  ╰────\n",
            "  help: 3 values for 2 target column(s): add the missing columns to the column list, or drop the extra values\n",
        ),
    );
}

#[test]
fn insert_select_column_count_mismatch_rejected() {
    let db = setup();
    // Target has 2 columns, SELECT has 1.
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email) SELECT name FROM users"),
        AnalyzeError::SyntaxError(_),
        concat!(
            "INSERT has more target columns than expressions\n",
            "  ╭────\n",
            "1 │ INSERT INTO users (name, email) SELECT name FROM users\n",
            "  ·                          ──┬──\n",
            "  ·                            ╰─ no value for this column\n",
            "  ╰────\n",
            "  help: 2 target column(s) for 1 value(s): supply a value for every listed column\n",
        ),
    );
}

// ── DEFAULT keyword ─────────────────────────────────────────────────────────

#[test]
fn insert_values_with_default_keyword() {
    let db = setup();
    // `DEFAULT` replaces a VALUES item and adopts the target column's type.
    // Only the $p1 param surfaces in the bindings — DEFAULT is not a param.
    let s = db
        .analyze(
            "INSERT INTO users (name, email, role) VALUES ($p1, $p2, DEFAULT) \
             RETURNING id, role",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
        ],
    );
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn update_set_column_to_default() {
    let db = setup();
    // `UPDATE … SET col = DEFAULT` is PG's spelling for "reset to the
    // column default". The analyzer must accept it without erroring out.
    let s = db
        .analyze(
            "UPDATE users SET role = DEFAULT WHERE id = $p1 \
             RETURNING id, role",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
        ],
    );
    assert_params(&s, vec![p(int8())]);
}

// ── INSERT … ON CONFLICT ────────────────────────────────────────────────────

#[test]
fn insert_on_conflict_do_nothing() {
    let db = setup();
    // DO NOTHING with no RETURNING surfaces only as a param-typed statement.
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO NOTHING",
        )
        .unwrap();
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn insert_on_conflict_do_update_with_excluded() {
    let db = setup();
    // `EXCLUDED.name` refers to the row the INSERT was trying to add — the
    // analyzer must resolve it against the target table's schema so the
    // SET assignment type-checks.
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name \
             RETURNING id, name, email",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), c("email", text())],
    );
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn insert_on_conflict_do_nothing_returning_not_null_columns() {
    let db = setup();
    // `ON CONFLICT DO NOTHING RETURNING` only emits a row when the INSERT
    // actually inserts. Each returned row is a fresh INSERT, so NOT NULL
    // columns are still NOT NULL — the analyzer should NOT promote them
    // to nullable just because the statement might return zero rows.
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO NOTHING \
             RETURNING id, name, email",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), c("email", text())],
    );
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn insert_on_conflict_do_nothing_returning_star() {
    let db = setup();
    // `RETURNING *` over DO NOTHING — every column has its base nullability.
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO NOTHING RETURNING *",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("name", text()),
            c("email", text()),
            cn("age", int4()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
            cn("preferences", domain("public", "user_prefs", jsonb())),
            c("created_at", timestamptz()),
        ],
    );
}

#[test]
fn insert_on_conflict_do_update_with_param_expression() {
    let db = setup();
    // `SET age = EXCLUDED.age + $p3` mixes a column reference with a new
    // param — the param must be inferred as int4 (the column's type).
    let s = db
        .analyze(
            "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) \
             ON CONFLICT (email) DO UPDATE SET age = EXCLUDED.age + $p4 \
             RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text()), pn(int4()), p(int4())]);
}

#[test]
fn insert_on_conflict_do_update_set_unknown_column_in_value_rejected() {
    // A typo on the right-hand side of `ON CONFLICT ... DO UPDATE SET`
    // used to be swallowed by `let _ = infer_expr(...)`.
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) \
             ON CONFLICT (email) DO UPDATE SET name = ghost",
        ),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) ON CONFLICT (email) DO UPDATE SET name = ghost\n",
            "  ·                                                                                                      ──┬──\n",
            "  ·                                                                                                        ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn insert_on_conflict_do_update_where_unknown_column_rejected() {
    // A typo in the `ON CONFLICT ... WHERE` predicate used to be
    // swallowed by `let _ = infer_expr(...)`.
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) \
             ON CONFLICT (email) DO UPDATE SET name = 'x' WHERE ghost",
        ),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) ON CONFLICT (email) DO UPDATE SET name = 'x' WHERE ghost\n",
            "  ·                                                                                                                ──┬──\n",
            "  ·                                                                                                                  ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

// ── ON CONFLICT target validation — `pg_constraint` / `pg_index` not modeled ─
//
// In PG, `ON CONFLICT (col)` requires `col` to be covered by a UNIQUE or
// PRIMARY KEY constraint (or a unique index). Without those rows in the
// catalog, the analyzer can't validate the conflict target and silently
// accepts any column.

#[test]
fn on_conflict_on_non_unique_column_should_error() {
    // ON CONFLICT validation is a planner-time check in PG; pglite-socket's
    // wire `prepare` skips it. Opt out of the mirror.
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (name) DO NOTHING",
        ),
        AnalyzeError::InvalidColumnReference(_),
        "there is no unique or exclusion constraint matching the ON CONFLICT specification on table \"users\"",
    );
}

#[test]
fn on_conflict_on_nonexistent_constraint_name_should_error() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id   BIGINT PRIMARY KEY,
            slug TEXT NOT NULL
         );",
    )
    .unwrap();
    // PG: `constraint "nope" for table "t" does not exist`.
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
             ON CONFLICT ON CONSTRAINT nope DO NOTHING",
        ),
        AnalyzeError::UndefinedObject(_),
        "constraint \"nope\" for table \"t\" does not exist",
    );
}

#[test]
fn on_conflict_on_primary_key_column_is_accepted() {
    // Sanity: PRIMARY KEY columns are valid ON CONFLICT targets.
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (id) DO NOTHING RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn on_conflict_on_unique_column_is_accepted() {
    // The `email` column is declared `UNIQUE` in setup() — must match.
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO NOTHING RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn on_conflict_on_unknown_column_is_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (ghost) DO NOTHING",
        ),
        AnalyzeError::UndefinedColumn(_),
        "column \"ghost\" does not exist",
    );
}

#[test]
fn on_conflict_on_named_pk_constraint_is_accepted() {
    // PG auto-names PK constraints `<table>_pkey`.
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT ON CONSTRAINT users_pkey DO NOTHING RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn on_conflict_on_composite_unique_match_is_accepted() {
    // Composite UNIQUE constraint covers exactly (a, b) — ON CONFLICT (a, b)
    // and (b, a) both match.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            a INT NOT NULL,
            b INT NOT NULL,
            extra TEXT,
            UNIQUE (a, b)
         );",
    )
    .unwrap();
    db.analyze(
        "INSERT INTO t (a, b, extra) VALUES ($p1, $p2, $p3) \
         ON CONFLICT (a, b) DO NOTHING",
    )
    .unwrap();
    db.analyze(
        "INSERT INTO t (a, b, extra) VALUES ($p1, $p2, $p3) \
         ON CONFLICT (b, a) DO NOTHING",
    )
    .unwrap();
}

#[test]
fn on_conflict_on_partial_composite_unique_set_is_rejected() {
    // A two-column UNIQUE doesn't cover ON CONFLICT on a single column.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            a INT NOT NULL,
            b INT NOT NULL,
            UNIQUE (a, b)
         );",
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
fn on_conflict_do_nothing_without_target_is_accepted() {
    // `ON CONFLICT DO NOTHING` without an `(...)` target needs no
    // matching constraint.
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT DO NOTHING RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn on_conflict_after_alter_add_unique_is_accepted() {
    // ALTER TABLE ADD CONSTRAINT … UNIQUE retroactively makes a column a
    // valid ON CONFLICT target.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, slug TEXT NOT NULL);
         ALTER TABLE t ADD CONSTRAINT t_slug_key UNIQUE (slug);",
    )
    .unwrap();
    db.analyze(
        "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
         ON CONFLICT (slug) DO NOTHING",
    )
    .unwrap();
    db.analyze(
        "INSERT INTO t (id, slug) VALUES ($p1, $p2) \
         ON CONFLICT ON CONSTRAINT t_slug_key DO NOTHING",
    )
    .unwrap();
}

#[test]
fn on_conflict_on_check_constraint_name_is_rejected() {
    // PG: ON CONFLICT ON CONSTRAINT only accepts unique/PK/exclusion
    // constraints. A CHECK constraint name is not a valid target.
    // We currently lookup by name without enforcing contype, so this
    // documents the gap if the user wants to tighten it later.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, qty INT NOT NULL);
         ALTER TABLE t ADD CONSTRAINT t_qty_pos CHECK (qty > 0);",
    )
    .unwrap();
    // Sanity: the CHECK constraint exists in pg_constraint after the ALTER.
    let names: Vec<String> = db
        .pg_constraint_names_for_table("public", "t")
        .into_iter()
        .collect();
    assert!(
        names.iter().any(|n| n == "t_qty_pos"),
        "expected t_qty_pos in {names:?}"
    );
}

// ── GENERATED ALWAYS AS IDENTITY — `pg_attribute.attidentity` ──────────────

#[test]
fn insert_into_generated_always_as_identity_should_error() {
    // GENERATED ALWAYS is enforced at parse_analyze in PG (not row-level),
    // so `prepare` already errors with the wording our analyzer mirrors.
    let db = setup();
    assert_analyze_err!(
        db.analyze("INSERT INTO users (id, name, email) VALUES ($p1, $p2, $p3)"),
        AnalyzeError::GeneratedAlways(_),
        "cannot insert a non-DEFAULT value into column \"id\" (Column \"id\" is an identity column defined as GENERATED ALWAYS.)\n  help: Use OVERRIDING SYSTEM VALUE to override.\n",
    );
}

#[test]
fn overriding_system_value_on_table_without_identity_is_a_noop() {
    // PG silently accepts `OVERRIDING SYSTEM VALUE` against a table with
    // no identity columns (it's a no-op rather than a parser error). The
    // analyzer mirrors that behavior to keep `pg_sanity` aligned, even
    // though writing the clause on a non-identity table is almost always
    // a caller mistake.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE plain (
            id   INT,
            name TEXT
         );",
    )
    .unwrap();
    let s = db
        .analyze(
            "INSERT INTO plain (id, name) OVERRIDING SYSTEM VALUE \
             VALUES ($p1, $p2) RETURNING id, name",
        )
        .unwrap();
    assert_cols(&s, vec![cn("id", int4()), cn("name", text())]);
    assert_params(&s, vec![pn(int4()), pn(text())]);
}

#[test]
fn insert_into_generated_always_with_default_keyword_is_allowed() {
    // `id BIGINT GENERATED ALWAYS AS IDENTITY` — DEFAULT is the one value
    // that's always accepted, even without OVERRIDING.
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (id, name, email) VALUES (DEFAULT, $p1, $p2) RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn insert_into_generated_always_with_overriding_system_value_is_allowed() {
    // OVERRIDING SYSTEM VALUE explicitly opts into supplying a literal for
    // an identity-ALWAYS column.
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO users (id, name, email) OVERRIDING SYSTEM VALUE \
             VALUES ($p1, $p2, $p3) RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int8()), p(text()), p(text())]);
}

#[test]
fn insert_into_generated_by_default_as_identity_accepts_explicit_value() {
    // `BY DEFAULT` identity columns accept user-supplied values without
    // needing OVERRIDING — the override only matters for ALWAYS columns.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id   BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
         );",
    )
    .unwrap();
    let s = db
        .analyze("INSERT INTO t (id, name) VALUES ($p1, $p2) RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int8()), p(text())]);
}

#[test]
fn insert_select_into_generated_always_is_rejected() {
    // INSERT ... SELECT cannot supply DEFAULT. Without an explicit override
    // the SELECT cannot target an identity-ALWAYS column.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE src (n BIGINT NOT NULL);
         CREATE TABLE dst (
            id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
         );",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze("INSERT INTO dst (id, name) SELECT n, 'x' FROM src"),
        AnalyzeError::GeneratedAlways(_),
        "cannot insert a non-DEFAULT value into column \"id\" (Column \"id\" is an identity column defined as GENERATED ALWAYS.)\n  help: Use OVERRIDING SYSTEM VALUE to override.\n",
    );
}

#[test]
fn insert_select_into_generated_always_with_overriding_system_value_is_allowed() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE src (n BIGINT NOT NULL);
         CREATE TABLE dst (
            id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
         );",
    )
    .unwrap();
    let s = db
        .analyze(
            "INSERT INTO dst (id, name) OVERRIDING SYSTEM VALUE \
             SELECT n, 'x' FROM src RETURNING id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn update_set_generated_always_to_literal_is_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("UPDATE users SET id = $p1 WHERE id = $p2"),
        AnalyzeError::GeneratedAlways(_),
        "column \"id\" can only be updated to DEFAULT (Column \"id\" is an identity column defined as GENERATED ALWAYS.)",
    );
}

#[test]
fn update_set_generated_always_to_default_is_allowed() {
    // PG: `UPDATE … SET id = DEFAULT` resets the identity — only DEFAULT
    // is accepted on an ALWAYS column, never a literal.
    let db = setup();
    let s = db
        .analyze("UPDATE users SET id = DEFAULT WHERE id = $p1 RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn update_set_by_default_identity_to_literal_is_allowed() {
    // BY DEFAULT identity columns accept regular updates.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id   BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
         );",
    )
    .unwrap();
    let s = db
        .analyze("UPDATE t SET id = $p1 WHERE name = $p2 RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int8()), p(text())]);
}

#[test]
fn alter_table_add_identity_then_insert_is_rejected() {
    // `ALTER TABLE … ADD GENERATED ALWAYS AS IDENTITY` retroactively
    // turns a column into an identity-ALWAYS column — INSERT rules apply
    // from that point on.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id BIGINT NOT NULL, name TEXT NOT NULL);
         ALTER TABLE t ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze("INSERT INTO t (id, name) VALUES ($p1, $p2)"),
        AnalyzeError::GeneratedAlways(_),
        "cannot insert a non-DEFAULT value into column \"id\" (Column \"id\" is an identity column defined as GENERATED ALWAYS.)\n  help: Use OVERRIDING SYSTEM VALUE to override.\n",
    );
}

#[test]
fn alter_table_drop_identity_re_enables_direct_insert() {
    // After DROP IDENTITY, the column is just a NOT NULL BIGINT again —
    // direct INSERTs are accepted.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
         );
         ALTER TABLE t ALTER COLUMN id DROP IDENTITY;",
    )
    .unwrap();
    let s = db
        .analyze("INSERT INTO t (id, name) VALUES ($p1, $p2) RETURNING id")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int8()), p(text())]);
}

#[test]
fn alter_table_set_identity_changes_kind() {
    // `ALTER TABLE … SET GENERATED ALWAYS` upgrades an existing BY DEFAULT
    // identity column to ALWAYS. Subsequent direct INSERTs must error.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id   BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
         );
         ALTER TABLE t ALTER COLUMN id SET GENERATED ALWAYS;",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze("INSERT INTO t (id, name) VALUES ($p1, $p2)"),
        AnalyzeError::GeneratedAlways(_),
        "cannot insert a non-DEFAULT value into column \"id\" (Column \"id\" is an identity column defined as GENERATED ALWAYS.)\n  help: Use OVERRIDING SYSTEM VALUE to override.\n",
    );
}

#[test]
fn merge_insert_into_generated_always_is_rejected() {
    // `MERGE … WHEN NOT MATCHED THEN INSERT (id, …) VALUES (literal, …)`
    // is just an INSERT with the same identity-ALWAYS rules.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE src (n BIGINT NOT NULL, label TEXT NOT NULL);
         CREATE TABLE dst (
            id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
         );",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze(
            "MERGE INTO dst d USING src s ON d.id = s.n \
             WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.n, s.label)"
        ),
        AnalyzeError::GeneratedAlways(_),
        "cannot insert a non-DEFAULT value into column \"id\" (Column \"id\" is an identity column defined as GENERATED ALWAYS.)\n  help: Use OVERRIDING SYSTEM VALUE to override.\n",
    );
}

#[test]
fn merge_update_generated_always_is_rejected() {
    // PG raises this at planning time, but pglite-socket's `prepare`
    // sometimes truncates the message so the prefix can't be checked
    // reliably — opt out of the mirror and rely on the analyzer.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE src (n BIGINT NOT NULL, label TEXT NOT NULL);
         CREATE TABLE dst (
            id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
         );",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze(
            "MERGE INTO dst d USING src s ON d.id = s.n \
             WHEN MATCHED THEN UPDATE SET id = s.n"
        ),
        AnalyzeError::GeneratedAlways(_),
        "column \"id\" can only be updated to DEFAULT (Column \"id\" is an identity column defined as GENERATED ALWAYS.)",
    );
}

#[test]
fn on_conflict_do_update_set_generated_always_is_rejected() {
    // ON CONFLICT DO UPDATE goes through the UPDATE path — assigning a
    // literal to an identity-ALWAYS column is rejected the same way.
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO users (name, email) VALUES ($p1, $p2) \
             ON CONFLICT (email) DO UPDATE SET id = 99"
        ),
        AnalyzeError::GeneratedAlways(_),
        "column \"id\" can only be updated to DEFAULT (Column \"id\" is an identity column defined as GENERATED ALWAYS.)",
    );
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_update_returning_expression() {
    let db = setup();
    let sql = "UPDATE users SET age = $p1 WHERE id = $p2 \
               RETURNING id, COALESCE(age, 0) as safe_age, name || '!' as excited";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            // COALESCE in RETURNING.
            c("safe_age", int4()),
            // String concat in RETURNING.
            c("excited", text()),
        ],
    );
}

#[test]
fn stress_delete_returning_all_columns() {
    let db = setup();
    let sql = "DELETE FROM users WHERE id = $p1 \
               RETURNING id, name, email, age, created_at";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            c("name", text()),
            c("email", text()),
            cn("age", int4()),
            c("created_at", timestamptz()),
        ],
    );
}

#[test]
fn stress_insert_returning_star() {
    let db = setup();
    let sql = "INSERT INTO posts (user_id, title) VALUES ($p1, $p2) RETURNING *";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            c("user_id", int8()),
            c("title", text()),
            cn("body", text()),
            cn("published_at", timestamptz()),
        ],
    );
}

#[test]
fn stress_insert_minimal() {
    let db = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert_params(&info, vec![p(text()), p(text())]);
    assert!(!col(&info, "id").nullable);
}

// ── Torture ──────────────────────────────────────────────────────────────────

#[test]
fn torture_update_from_join() {
    let db = setup();
    let sql = "UPDATE posts SET body = $p1 \
               FROM users u \
               WHERE posts.user_id = u.id AND u.name = $p2 \
               RETURNING posts.id, posts.title, posts.body";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![c("id", int8()), c("title", text()), cn("body", text())],
    );
}

// ── MERGE (PG 15+) ───────────────────────────────────────────────────────────

#[test]
fn merge_when_matched_update() {
    let db = setup();
    // Classic upsert via MERGE. RETURNING preserves the target table's
    // base nullabilities — `id` and `name` NOT NULL, `age` nullable.
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::bigint AS id, $p2::text AS name) src \
             ON u.id = src.id \
             WHEN MATCHED THEN UPDATE SET name = src.name \
             WHEN NOT MATCHED THEN INSERT (name, email) VALUES (src.name, $p3) \
             RETURNING u.id, u.name",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int8()), p(text()), p(text())]);
}

#[test]
fn merge_when_not_matched_insert() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::text AS email) src \
             ON u.email = src.email \
             WHEN NOT MATCHED THEN INSERT (name, email) VALUES ($p2, src.email)",
        )
        .unwrap();
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn merge_when_matched_delete() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::bigint AS id) src \
             ON u.id = src.id \
             WHEN MATCHED THEN DELETE \
             RETURNING u.id, u.name",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn merge_when_matched_with_extra_condition() {
    let db = setup();
    // `WHEN MATCHED AND age > $p2 THEN ...` — the extra condition must
    // be walked as a BOOL predicate; its placeholder gets `int4` from the
    // column comparison, not a `text` fallback.
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::bigint AS id) src \
             ON u.id = src.id \
             WHEN MATCHED AND u.age > $p2 THEN UPDATE SET name = 'aged'",
        )
        .unwrap();
    assert_params(&s, vec![p(int8()), p(int4())]);
}

#[test]
fn merge_with_multiple_set_columns() {
    let db = setup();
    // UPDATE with several SET targets: each value gets its own column-typed
    // assignment goal. `age` is nullable on the table — passing `$p3` to it
    // should infer the param as nullable.
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::bigint AS id) src \
             ON u.id = src.id \
             WHEN MATCHED THEN UPDATE SET name = $p2, age = $p3",
        )
        .unwrap();
    assert_params(&s, vec![p(int8()), p(text()), pn(int4())]);
}

#[test]
fn merge_when_not_matched_do_nothing() {
    let db = setup();
    // `WHEN NOT MATCHED THEN DO NOTHING` — no target / value walk, just
    // the optional condition (none here). Source-side params still get
    // inferred from their explicit casts.
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::text AS email) src \
             ON u.email = src.email \
             WHEN NOT MATCHED THEN DO NOTHING",
        )
        .unwrap();
    assert_params(&s, vec![p(text())]);
}

#[test]
fn merge_with_cte() {
    let db = setup();
    // A WITH clause feeding the MERGE source. The CTE must be visible to
    // both the source FROM-item and the join condition.
    let s = db
        .analyze(
            "WITH new_emails AS (SELECT $p1::text AS email) \
             MERGE INTO users u \
             USING new_emails src \
             ON u.email = src.email \
             WHEN NOT MATCHED THEN INSERT (name, email) VALUES ($p2, src.email)",
        )
        .unwrap();
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn merge_without_returning_yields_no_columns() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::bigint AS id) src \
             ON u.id = src.id \
             WHEN MATCHED THEN DELETE",
        )
        .unwrap();
    assert_cols(&s, vec![]);
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn merge_update_unknown_column_errors() {
    let db = setup();
    // Same rule the analyzer enforces for plain UPDATE: an unknown column
    // in `WHEN MATCHED THEN UPDATE SET ghost = …` must surface clearly,
    // not get silently typed as text via the UNKNOWN fallback.
    assert_analyze_err!(
        db.analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::bigint AS id) src \
             ON u.id = src.id \
             WHEN MATCHED THEN UPDATE SET ghost = 'x'"
        ),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" of relation \"users\" does not exist\n",
            "  ╭────\n",
            "1 │ MERGE INTO users u USING (SELECT $p1::bigint AS id) src ON u.id = src.id WHEN MATCHED THEN UPDATE SET ghost = 'x'\n",
            "  ·                                                                                                       ──┬──\n",
            "  ·                                                                                                         ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn merge_insert_null_into_not_null_column_errors() {
    // MERGE that ends up doing an INSERT — pg_sanity's execute fallback
    // hits the row-level NOT NULL check and PG's wording matches ours.
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::text AS email) src \
             ON u.email = src.email \
             WHEN NOT MATCHED THEN INSERT (name, email) VALUES (NULL, src.email)"
        ),
        AnalyzeError::Invalid(_),
        "null value in column \"name\" of relation \"users\" violates not-null constraint (cannot insert NULL into NOT NULL column `users.name`)",
    );
}

#[test]
fn merge_update_set_not_null_to_null_literal_errors() {
    // MERGE that ends up doing an UPDATE — the execute fallback runs with
    // NULL params against an empty scratch table, so the WHEN MATCHED arm
    // never fires and the row-level NOT NULL check stays out of reach.
    // Keep the skip and rely on the analyzer's compile-time guard.
    let mut db = setup();
    db.skip_pg_sanity();
    assert_analyze_err!(
        db.analyze(
            "MERGE INTO users u \
             USING (SELECT $p1::bigint AS id) src \
             ON u.id = src.id \
             WHEN MATCHED THEN UPDATE SET name = NULL"
        ),
        AnalyzeError::Invalid(_),
        "null value in column \"name\" of relation \"users\" violates not-null constraint (cannot assign NULL to NOT NULL column `users.name`)",
    );
}

#[test]
fn merge_with_real_table_as_source() {
    let db = setup();
    // Source is a real table (not a subquery), joined on a non-trivial
    // predicate. RETURNING projects target columns with their base
    // nullability — `body` is nullable on `posts`-comparable shape, but
    // here we project from `users` so `id` / `name` stay NOT NULL.
    let s = db
        .analyze(
            "MERGE INTO users u \
             USING posts p \
             ON p.user_id = u.id AND p.title = $p1 \
             WHEN MATCHED THEN UPDATE SET name = p.title \
             RETURNING u.id, u.name",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(text())]);
}

#[test]
fn torture_expression_in_insert_returning() {
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) \
               RETURNING id, \
                         name || ' (' || email || ')' as display, \
                         CASE WHEN age >= 18 THEN true ELSE false END as is_adult";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            // Concat of NOT NULL → NOT NULL.
            c("display", text()),
            // CASE with ELSE, all literal booleans → NOT NULL.
            c("is_adult", bool_ty()),
        ],
    );
}

// ── WITH (CTE) on INSERT / UPDATE / DELETE ───────────────────────────────────
//
// CTEs attached directly to a DML statement (`WITH x AS (…) INSERT …`) are
// parsed into the DML node's own `with_clause`, not into the inner SELECT.
// The analyzer must walk that clause so parameters used only inside the CTE
// are seen — otherwise `into_sorted` reports a "parameter gap" because lower
// param numbers are missing from the seen set.

#[test]
fn insert_with_cte_select_param_is_seen() {
    let db = setup();
    let s = db
        .analyze(
            "WITH src AS (SELECT $p1::text AS author) \
             INSERT INTO comments (post_id, author_name, content) \
             SELECT $p2, src.author, $p3 FROM src",
        )
        .unwrap();
    // $p1 is consumed inside the CTE; without walking with_clause the
    // analyzer would only see $p2/$p3 and complain about a gap at $p1.
    assert_params(&s, vec![p(text()), p(int8()), p(text())]);
}

#[test]
fn insert_with_cte_updating_table_reuses_param() {
    let db = setup();
    // Mirrors the user-reported case: a data-modifying CTE feeds an INSERT,
    // and the same named param ($p1) appears both inside the CTE and in the
    // outer SELECT. The lexer dedupes to `$1` so the rewritten SQL stays
    // valid; the analyzer must walk the CTE so $p2 (only used in the CTE)
    // doesn't get skipped.
    let s = db
        .analyze(
            "WITH bump AS ( \
                 UPDATE posts SET body = $p3 \
                 WHERE id = $p1 AND user_id = $p2 \
                 RETURNING id, user_id \
             ) \
             INSERT INTO comments (post_id, author_name, content) \
             SELECT $p1, $p4, $p5 FROM bump",
        )
        .unwrap();
    // Lexer assigns positional numbers in the order each named param first
    // appears: $p3 (SET body), $p1 (WHERE id), $p2 (user_id), $p4, $p5.
    // body is nullable, so $p3 is inferred nullable.
    assert_params(
        &s,
        vec![pn(text()), p(int8()), p(int8()), p(text()), p(text())],
    );
}

#[test]
fn update_with_cte_param_is_seen() {
    let db = setup();
    let s = db
        .analyze(
            "WITH src AS (SELECT $p1::int AS bump) \
             UPDATE users SET age = age + src.bump FROM src WHERE id = $p2",
        )
        .unwrap();
    assert_params(&s, vec![p(int4()), p(int8())]);
}

#[test]
fn delete_with_cte_param_is_seen() {
    let db = setup();
    // CTEs attached to a DELETE land in `DeleteStmt.with_clause` (not the
    // WHERE sublink). The analyzer must walk it so $p1 — only referenced
    // inside the CTE — gets registered. The CTE here is unused by the body
    // (PG emits a NOTICE but accepts it); we use this minimal shape to
    // isolate the with_clause walk from the separate question of whether
    // sublinks resolve outer CTEs.
    let s = db
        .analyze(
            "WITH dead AS (SELECT $p1::bigint AS id) \
             DELETE FROM comments WHERE rating < $p2",
        )
        .unwrap();
    assert_params(&s, vec![p(int8()), p(int4())]);
}

// ── can_run_as_subquery ──────────────────────────────────────────────────────
//
// Drives the `SELECT * FROM (<query>) LIMIT 2` wrap that the codegen applies
// to fetch_one / fetch_optional. Must be true only when PG accepts the query
// as a subquery body — top-level DML and `WITH (DML) SELECT` both fail
// (`E0A000: WITH clause containing a data-modifying statement must be at the
// top level`), so they are sent unwrapped.

#[test]
fn can_run_as_subquery_plain_select() {
    let db = setup();
    let s = db.analyze("SELECT id, name FROM users").unwrap();
    assert!(s.can_run_as_subquery);
}

#[test]
fn can_run_as_subquery_with_pure_select_cte() {
    let db = setup();
    let s = db
        .analyze(
            "WITH adults AS (SELECT id FROM users WHERE age >= 18) \
             SELECT id FROM adults",
        )
        .unwrap();
    assert!(s.can_run_as_subquery);
}

#[test]
fn can_run_as_subquery_values_clause() {
    let db = setup();
    // `VALUES (…)` parses as a SelectStmt; safe to wrap.
    let s = db.analyze("VALUES (1, 'a'), (2, 'b')").unwrap();
    assert!(s.can_run_as_subquery);
}

#[test]
fn can_run_as_subquery_top_level_update_returning() {
    let db = setup();
    let s = db
        .analyze("UPDATE users SET age = $p1 WHERE id = $p2 RETURNING id, age")
        .unwrap();
    assert!(!s.can_run_as_subquery);
}

#[test]
fn can_run_as_subquery_top_level_insert_returning() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id")
        .unwrap();
    assert!(!s.can_run_as_subquery);
}

#[test]
fn can_run_as_subquery_top_level_delete_returning() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM users WHERE id = $p1 RETURNING id")
        .unwrap();
    assert!(!s.can_run_as_subquery);
}

#[test]
fn can_run_as_subquery_with_update_cte_select_body() {
    let db = setup();
    // The bug case: top-level SELECT looks wrappable, but the CTE contains
    // an UPDATE — PG only accepts data-modifying CTEs at the top level.
    let s = db
        .analyze(
            "WITH bumped AS ( \
                 UPDATE users SET age = age + 1 \
                 WHERE id = $p1 \
                 RETURNING id, age \
             ) \
             SELECT b.id, b.age, u.email \
             FROM bumped b JOIN users u ON u.id = b.id",
        )
        .unwrap();
    assert!(!s.can_run_as_subquery);
}

#[test]
fn can_run_as_subquery_with_insert_cte_select_body() {
    let db = setup();
    let s = db
        .analyze(
            "WITH ins AS ( \
                 INSERT INTO users (name, email) VALUES ($p1, $p2) \
                 RETURNING id \
             ) \
             SELECT id FROM ins",
        )
        .unwrap();
    assert!(!s.can_run_as_subquery);
}

#[test]
fn can_run_as_subquery_with_delete_cte_select_body() {
    let db = setup();
    let s = db
        .analyze(
            "WITH gone AS ( \
                 DELETE FROM comments WHERE rating < $p1 \
                 RETURNING id, post_id \
             ) \
             SELECT id FROM gone",
        )
        .unwrap();
    assert!(!s.can_run_as_subquery);
}

// ── UndefinedTable in DML statements ────────────────────────────────────────
//
// Each DML statement type (INSERT, UPDATE, DELETE, MERGE) carries its target
// relation on its own AST node, with a separate analyzer code path. The
// diagnostic must point at the exact relation reference regardless of which
// statement kind is failing.

#[test]
fn undefined_table_in_insert_renders_snippet_and_hint() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("INSERT INTO userz (name, email) VALUES ('a', 'b')"),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"userz\" does not exist
  ╭────
1 │ INSERT INTO userz (name, email) VALUES ('a', 'b')
  ·             ──┬──
  ·               ╰─ relation does not exist
  ╰────
  help: did you mean \"users\"?\n",
    );
}

#[test]
fn undefined_table_in_update_renders_snippet_and_hint() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("UPDATE userz SET name = 'y' WHERE id = 1"),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"userz\" does not exist
  ╭────
1 │ UPDATE userz SET name = 'y' WHERE id = 1
  ·        ──┬──
  ·          ╰─ relation does not exist
  ╰────
  help: did you mean \"users\"?\n",
    );
}

#[test]
fn undefined_table_in_delete_renders_snippet_and_hint() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("DELETE FROM userz WHERE id = 1"),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"userz\" does not exist
  ╭────
1 │ DELETE FROM userz WHERE id = 1
  ·             ──┬──
  ·               ╰─ relation does not exist
  ╰────
  help: did you mean \"users\"?\n",
    );
}

#[test]
fn undefined_table_in_merge_renders_snippet_and_hint() {
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "MERGE INTO userz AS t USING posts p ON t.id = p.user_id \
             WHEN MATCHED THEN DELETE"
        ),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"userz\" does not exist
  ╭────
1 │ MERGE INTO userz AS t USING posts p ON t.id = p.user_id WHEN MATCHED THEN DELETE
  ·            ──┬──
  ·              ╰─ relation does not exist
  ╰────
  help: did you mean \"users\"?\n",
    );
}

#[test]
fn undefined_table_in_update_from_locates_secondary_relation() {
    // `UPDATE ... FROM <bad>` — the target exists, but the FROM list adds a
    // missing relation. Hits the `process_from_item` → `add_table` path
    // inside the UPDATE analyzer rather than the target check.
    let db = setup();
    assert_analyze_err!(
        db.analyze("UPDATE users SET name = p.title FROM postz p WHERE p.user_id = users.id"),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"postz\" does not exist
  ╭────
1 │ UPDATE users SET name = p.title FROM postz p WHERE p.user_id = users.id
  ·                                      ──┬──
  ·                                        ╰─ relation does not exist
  ╰────
  help: did you mean \"posts\"?\n",
    );
}

#[test]
fn undefined_table_multiline_update_from_locates_offending_line() {
    // A real-world-looking multi-line UPDATE with the failure on a line
    // far from the start. Confirms that line/column pin to the right line
    // and the snippet shows only that line (the rest of the SQL is elided).
    let db = setup();
    let sql = "UPDATE users\n   SET name = p.title\n  FROM postz p\n WHERE p.user_id = users.id";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"postz\" does not exist
  ╭────
3 │   FROM postz p
  ·        ──┬──
  ·          ╰─ relation does not exist
  ╰────
  help: did you mean \"posts\"?\n",
    );
}

// ── TypeMismatch: full rendered diagnostic ─────────────────────────────────

#[test]
fn type_mismatch_insert_int_into_bool_column() {
    // Adding a BOOL column to `users` and supplying an integer literal —
    // the snippet pinpoints the literal and the caret label spells out
    // the actual vs expected types.
    let mut db = setup();
    db.apply_sql("ALTER TABLE users ADD COLUMN flag BOOLEAN NOT NULL DEFAULT false")
        .unwrap();
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email, flag) VALUES ('a', 'b', 42)"),
        AnalyzeError::TypeMismatch { .. },
        "\
column \"flag\" is of type boolean but expression is of type integer
  ╭────
1 │ INSERT INTO users (name, email, flag) VALUES ('a', 'b', 42)
  ·                                 ──┬─                    ─┬
  ·                                   │                      ╰─ expected boolean, found integer
  ·                                   ╰─ expected boolean here
  ╰────
  help: You will need to rewrite or cast the expression.
  note: an explicit cast from integer to boolean exists: `expr::boolean`
",
    );
}

#[test]
fn type_mismatch_insert_bool_into_bigint_column() {
    // Reverse direction: BOOL literal where BIGINT is expected.
    let db = setup();
    assert_analyze_err!(
        db.analyze("INSERT INTO posts (user_id, title) VALUES (true, 'hi')"),
        AnalyzeError::TypeMismatch { .. },
        "\
column \"user_id\" is of type bigint but expression is of type boolean
  ╭────
1 │ INSERT INTO posts (user_id, title) VALUES (true, 'hi')
  ·                    ───┬───                 ──┬─
  ·                       │                      ╰─ expected bigint, found boolean
  ·                       ╰─ expected bigint here
  ╰────
  help: You will need to rewrite or cast the expression.
",
    );
}

// ── INSERT … SELECT propagates errors from inside the SELECT ───────────────
//
// Earlier the analyzer swallowed errors raised inside an `INSERT … SELECT`
// SELECT (typos in JOIN ON, typos in the target list, …). The downstream
// invariant `param count mismatch` would then fire because the lexer had
// seen `$N` placeholders that the swallowed walk never registered.

#[test]
fn insert_select_typo_in_join_on_propagates() {
    let db = setup();
    // `p.user_idz` doesn't exist on `posts`; the analyzer must surface the
    // UndefinedColumn rather than failing the param-count invariant later.
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO comments (post_id, author_name, content) \
             SELECT p.id, u.name, 'hi' FROM posts p JOIN users u ON u.id = p.user_idz \
             WHERE p.id = $pid",
        ),
        AnalyzeError::UndefinedColumn(_),
        "\
column p.user_idz does not exist
  ╭────
1 │ INSERT INTO comments (post_id, author_name, content) SELECT p.id, u.name, 'hi' FROM posts p JOIN users u ON u.id = p.user_idz WHERE p.id = $pid
  ·                                                                                                                    ─────┬────
  ·                                                                                                                         ╰─ column does not exist
  ╰────
  help: Perhaps you meant to reference the column \"p.user_id\".
",
    );
}

#[test]
fn insert_select_typo_in_select_list_propagates() {
    // Typo in the SELECT target list (`p.user_idz` instead of `p.user_id`)
    // must reach the user as UndefinedColumn — previously silently aliased
    // to text via the UNKNOWN fallback.
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO comments (post_id, author_name, content) \
             SELECT p.id, p.user_idz::text, 'hi' FROM posts p",
        ),
        AnalyzeError::UndefinedColumn(_),
        "\
column p.user_idz does not exist
  ╭────
1 │ INSERT INTO comments (post_id, author_name, content) SELECT p.id, p.user_idz::text, 'hi' FROM posts p
  ·                                                                   ─────┬────
  ·                                                                        ╰─ column does not exist
  ╰────
  help: Perhaps you meant to reference the column \"p.user_id\".
",
    );
}

// ── Non-boolean WHERE in DML uses PG's wording (SQLSTATE 42804) ──────────────

#[test]
fn delete_where_non_boolean_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("DELETE FROM users WHERE age"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of WHERE must be type boolean, not type integer\n",
            "  ╭────\n",
            "1 │ DELETE FROM users WHERE age\n",
            "  ·                         ─┬─\n",
            "  ·                          ╰─ this is integer, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn update_where_non_boolean_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("UPDATE users SET name = 'x' WHERE age"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of WHERE must be type boolean, not type integer\n",
            "  ╭────\n",
            "1 │ UPDATE users SET name = 'x' WHERE age\n",
            "  ·                                   ─┬─\n",
            "  ·                                    ╰─ this is integer, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

// ── Assignment coerce-via-I/O to string-category targets (PG asymmetric rule) ─

#[test]
fn update_int_into_text_column_accepted() {
    // PG coerces any scalar to a **string-category** target via its I/O
    // functions in assignment context, so `UPDATE … SET text_col = 780` is
    // valid. The analyzer used to reject it as a type mismatch.
    let db = setup();
    let r = db.analyze("UPDATE posts SET title = 780 WHERE id = 1");
    assert!(
        r.is_ok(),
        "expected int→text assignment to be accepted, got: {r:?}"
    );
}

#[test]
fn update_text_into_int_column_rejected() {
    // The coerce-via-I/O rule is asymmetric: a string *source* assigned to a
    // non-string target needs an *explicit* cast, so `int_col = text_col` is
    // still rejected. PG: `column "age" is of type integer but expression is of
    // type text`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("UPDATE users SET age = name WHERE id = 1"),
        AnalyzeError::TypeMismatch { .. },
        concat!(
            "column \"age\" is of type integer but expression is of type text\n",
            "  ╭────\n",
            "1 │ UPDATE users SET age = name WHERE id = 1\n",
            "  ·                  ─┬─   ──┬─\n",
            "  ·                   │      ╰─ expected integer, found text\n",
            "  ·                   ╰─ expected integer here\n",
            "  ╰────\n",
            "  help: You will need to rewrite or cast the expression.\n",
            "  note: an explicit cast from text to integer exists: `expr::integer`\n",
        ),
    );
}

// ── INSERT … SELECT assignment type checking ────────────────────────────────

#[test]
fn insert_select_incompatible_column_type_rejected() {
    // PG checks each SELECT output column against the target column at parse
    // time: jsonb has no assignment cast to bigint.
    let db = setup();
    let err = db
        .analyze("INSERT INTO posts (user_id, title) SELECT preferences, name FROM users")
        .unwrap_err();
    assert!(
        err.to_string().starts_with(
            "column \"user_id\" is of type bigint but expression is of type user_prefs"
        ),
        "got: {err}"
    );
}

#[test]
fn insert_select_coercible_column_types_accepted() {
    // int4 → int8 is an implicit cast; text stays text.
    let db = setup();
    db.analyze("INSERT INTO posts (user_id, title) SELECT age, name FROM users")
        .unwrap();
}

#[test]
fn insert_select_unknown_literal_content_validated() {
    // A bare string literal in the SELECT projection is coerced through the
    // target column's input function, exactly like a VALUES literal.
    let db = setup();
    let err = db
        .analyze("INSERT INTO posts (user_id) SELECT 'x' FROM users")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("invalid input syntax for type bigint: \"x\""),
        "got: {err}"
    );
    db.analyze("INSERT INTO posts (user_id) SELECT '42' FROM users")
        .unwrap();
}

// ── DEFAULT placement (transformAssignedExpr) ───────────────────────────────

#[test]
fn default_only_allowed_as_a_whole_assigned_value() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE td (n int PRIMARY KEY DEFAULT 1, ni int, arr int[]);")
        .unwrap();
    for sql in [
        "SELECT DEFAULT",
        "SELECT n FROM td WHERE ni = DEFAULT",
        "INSERT INTO td (n) VALUES (DEFAULT + 1)",
        "UPDATE td SET ni = COALESCE(DEFAULT, 1)",
        "VALUES (DEFAULT)",
        "INSERT INTO td (n, ni) SELECT 1, DEFAULT",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::SyntaxError(_),
            "DEFAULT is not allowed in this context"
        );
    }
    assert_err_prefix!(
        db.analyze("UPDATE td SET arr[1] = DEFAULT"),
        AnalyzeError::FeatureNotSupported(_),
        "cannot set an array element to DEFAULT"
    );
    for sql in [
        "INSERT INTO td VALUES (DEFAULT, DEFAULT, DEFAULT), (1, DEFAULT, NULL)",
        "UPDATE td SET ni = DEFAULT",
        "INSERT INTO td (n) VALUES ((DEFAULT))",
        "INSERT INTO td (n) VALUES (1) ON CONFLICT (n) DO UPDATE SET ni = DEFAULT",
        "MERGE INTO td USING (SELECT 1 AS x) s ON td.n = s.x \
         WHEN MATCHED THEN UPDATE SET ni = DEFAULT \
         WHEN NOT MATCHED THEN INSERT VALUES (DEFAULT)",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn insert_without_column_list_may_supply_fewer_values() {
    // transformInsertRow: without a column list the trailing columns take
    // their defaults; with one, every listed column needs a value (PG 18).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE ta (j jsonb NOT NULL, n int NOT NULL DEFAULT 1, ni int);")
        .unwrap();
    for sql in [
        "INSERT INTO ta VALUES ('{}')",
        "INSERT INTO ta VALUES ('{}', 2), ('[]', 3)",
        "INSERT INTO ta SELECT '{}'::jsonb",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (sql, msg) in [
        (
            "INSERT INTO ta (j, n) VALUES ('{}')",
            "INSERT has more target columns than expressions",
        ),
        (
            "INSERT INTO ta VALUES ('{}', 1, 2, 3)",
            "INSERT has more expressions than target columns",
        ),
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
}

// ── INSERT … VALUES with ORDER BY / LIMIT / locking / WITH ──────────────────

/// transformInsertStmt's `isGeneralSelect`: a VALUES source carrying ORDER
/// BY, LIMIT / OFFSET, a locking clause or its own WITH is analyzed as a
/// standalone SELECT, its output columns then assigned to the targets.
fn general_values_db() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE gv (x int, y text); CREATE TABLE gvn (x int NOT NULL);")
        .unwrap();
    db
}

#[test]
fn insert_values_with_select_clauses_reports_the_select_errors() {
    let db = general_values_db();
    for (sql, msg) in [
        (
            "INSERT INTO gv (x) VALUES (1) ORDER BY nonexistent",
            "column \"nonexistent\" does not exist",
        ),
        (
            "INSERT INTO gv (x) VALUES (1) ORDER BY column1 LIMIT 'x'",
            "invalid input syntax for type bigint: \"x\"",
        ),
        (
            "INSERT INTO gv (x) VALUES (1) LIMIT sum(1)",
            "aggregate functions are not allowed in LIMIT",
        ),
        (
            "INSERT INTO gv (x) VALUES (1) ORDER BY 2",
            "ORDER BY position 2 is not in select list",
        ),
        // The target's columns are not in the VALUES' scope.
        (
            "INSERT INTO gv (x) VALUES (1) LIMIT x",
            "column \"x\" does not exist",
        ),
        (
            "INSERT INTO gv (x) VALUES (DEFAULT) LIMIT 1",
            "DEFAULT is not allowed in this context",
        ),
        (
            "INSERT INTO gv (x) VALUES (generate_series(1, 2)) LIMIT 5",
            "set-returning functions are not allowed in VALUES",
        ),
        (
            "INSERT INTO gv (x) VALUES (1) FOR UPDATE",
            "FOR UPDATE cannot be applied to VALUES",
        ),
        (
            "INSERT INTO gv (x) VALUES (1, 2) LIMIT 1",
            "INSERT has more expressions than target columns",
        ),
        (
            "INSERT INTO gv (x, y) VALUES (1) LIMIT 1",
            "INSERT has more target columns than expressions",
        ),
        (
            "INSERT INTO gv VALUES (1), (2, 'b') LIMIT 1",
            "VALUES lists must all be the same length",
        ),
        // The VALUES' columns are resolved on their own: an untyped literal,
        // NULL or parameter alone in its column is text.
        (
            "INSERT INTO gv (x) VALUES ('1') ORDER BY 1",
            "column \"x\" is of type integer but expression is of type text",
        ),
        (
            "INSERT INTO gvn VALUES (NULL) LIMIT 1",
            "column \"x\" is of type integer but expression is of type text",
        ),
        (
            "INSERT INTO gv (x) VALUES ($1) LIMIT 1",
            "column \"x\" is of type integer but expression is of type text",
        ),
        (
            "INSERT INTO gv (x) VALUES ('2020-01-01'::date) OFFSET 0",
            "column \"x\" is of type integer but expression is of type date",
        ),
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
}

#[test]
fn insert_values_with_select_clauses_is_accepted_like_pg() {
    let db = general_values_db();
    for sql in [
        "INSERT INTO gv (x) VALUES (1) OFFSET 0",
        "INSERT INTO gv (x) VALUES (2), (1) ORDER BY column1 LIMIT 1",
        "INSERT INTO gv (x) VALUES (1) ORDER BY column1 + 1",
        "INSERT INTO gv (x) VALUES (1) LIMIT (SELECT max(x) FROM gv)",
        "INSERT INTO gv (x) WITH c AS (SELECT 1 AS v) VALUES ((SELECT v FROM c))",
        "INSERT INTO gv (x) VALUES (1.5::numeric) LIMIT 1",
        "INSERT INTO gv (y) VALUES (1) LIMIT 1",
        "INSERT INTO gv (x, y) VALUES (NULL::int, NULL) LIMIT 1",
        // The common type of the rows is int: the NULL row may be cut by
        // the LIMIT, and is no literal NULL assigned to the column.
        "INSERT INTO gvn (x) VALUES (1), (NULL) LIMIT 1",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn insert_values_with_select_clauses_types_params_like_pg() {
    let db = general_values_db();
    // A parameter alone in its column is text; next to an int it is int.
    // Either may be NULL when its target column is nullable, as in a plain
    // VALUES list.
    let s = db
        .analyze("INSERT INTO gv (y) VALUES ($1) LIMIT 1")
        .unwrap();
    assert_params(&s, vec![pn(text())]);
    let s = db
        .analyze("INSERT INTO gv (x) VALUES ($1), (2) LIMIT 1")
        .unwrap();
    assert_params(&s, vec![pn(int4())]);
    let s = db
        .analyze("INSERT INTO gvn (x) VALUES ($1), (2) LIMIT 1")
        .unwrap();
    assert_params(&s, vec![p(int4())]);
    // LIMIT's own parameter is bigint.
    let s = db
        .analyze("INSERT INTO gv (x) VALUES (1) LIMIT $1 RETURNING x")
        .unwrap();
    assert_params(&s, vec![p(int8())]);
    assert_cols(&s, vec![c("x", int4())]);
}
