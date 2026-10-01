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
fn insert_values_row_wrong_arity_rejected() {
    let db = setup();
    // Explicit column list (name, email) expects 2 values per row; we
    // pass 3. PG: `INSERT has more expressions than target columns`.
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email) VALUES ($p1, $p2, $p3)"),
        AnalyzeError::Invalid(_),
        "INSERT has more expressions than target columns (table `users` expects 2, got 3)",
    );
}

#[test]
fn insert_select_column_count_mismatch_rejected() {
    let db = setup();
    // Target has 2 columns, SELECT has 1.
    assert_analyze_err!(
        db.analyze("INSERT INTO users (name, email) SELECT name FROM users"),
        AnalyzeError::Invalid(_),
        "INSERT has more target columns than expressions (table `users` expects 2, SELECT produces 1)",
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
  help: did you mean \"user_id\"?
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
  help: did you mean \"user_id\"?
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
