//! SELECT feature: projection, `*`, alias, table qualification,
//! DISTINCT / DISTINCT ON, LIMIT / OFFSET.

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

// ── LIMIT / OFFSET require int8; non-int8 expressions are rejected ───────────

#[test]
fn limit_bool_literal_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users LIMIT true"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of LIMIT must be type bigint, not type boolean\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users LIMIT true\n",
            "  ·                            ────\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn limit_text_column_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users LIMIT name"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of LIMIT must be type bigint, not type text\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users LIMIT name\n",
            "  ·                            ────\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn limit_timestamptz_column_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users LIMIT created_at"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of LIMIT must be type bigint, not type timestamp with time zone\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users LIMIT created_at\n",
            "  ·                            ──────────\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn offset_bool_literal_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users OFFSET false"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of OFFSET must be type bigint, not type boolean\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users OFFSET false\n",
            "  ·                             ─────\n",
            "  ╰────\n",
        ),
    );
}

// ── Basic SELECT ─────────────────────────────────────────────────────────────

#[test]
fn simple_select() {
    let db = setup();
    let s = db.analyze("SELECT id, name, age FROM users").unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn select_with_params() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users WHERE age > $p1 AND name = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int4()), p(text())]);
}

#[test]
fn select_star() {
    let db = setup();
    let s = db.analyze("SELECT * FROM users").unwrap();
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
fn select_star_from_posts() {
    let db = setup();
    let s = db.analyze("SELECT * FROM posts").unwrap();
    assert_cols(
        &s,
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
fn select_star_from_comments() {
    let db = setup();
    let s = db.analyze("SELECT * FROM comments").unwrap();
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
}

#[test]
fn select_aliased_columns() {
    let db = setup();
    let s = db
        .analyze("SELECT id AS user_id, name AS user_name FROM users")
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), c("user_name", text())]);
}

#[test]
fn select_table_qualified() {
    let db = setup();
    let s = db
        .analyze("SELECT users.id, users.name FROM users")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn select_alias_qualified() {
    let db = setup();
    let s = db
        .analyze("SELECT u.id, u.name, u.age FROM users u")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn select_all_columns_explicit() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name, email, age, created_at FROM users")
        .unwrap();
    assert_cols(
        &s,
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
fn nullable_column() {
    let db = setup();
    let s = db.analyze("SELECT id, age FROM users").unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("age", int4())]);
}

// ── ORDER BY / LIMIT / OFFSET ────────────────────────────────────────────────

#[test]
fn order_by() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users ORDER BY name ASC, id DESC")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn limit_offset_literals() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users ORDER BY id LIMIT 10 OFFSET 5")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn limit_offset_params() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users ORDER BY id LIMIT $p1 OFFSET $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    // LIMIT/OFFSET take int8.
    assert_params(&s, vec![p(int8()), p(int8())]);
}

// ── DISTINCT / DISTINCT ON ───────────────────────────────────────────────────

#[test]
fn select_distinct() {
    let db = setup();
    let s = db.analyze("SELECT DISTINCT name FROM users").unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn distinct_on() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT DISTINCT ON (user_id) user_id, title \
             FROM posts ORDER BY user_id, published_at DESC NULLS LAST",
        )
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), c("title", text())]);
}

// ── Stress (nullability focus) ───────────────────────────────────────────────

#[test]
fn stress_star_with_left_join() {
    let db = setup();
    // SELECT * from LEFT JOIN — right side columns should be nullable.
    let sql = "SELECT u.id, u.name, p.title, p.body \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            c("name", text()),
            // title is NOT NULL in table but LEFT JOIN makes it nullable.
            cn("title", text()),
            // body is nullable in table AND LEFT JOIN.
            cn("body", text()),
        ],
    );
}

#[test]
fn stress_select_without_from() {
    let db = setup();
    let sql = "SELECT 1 as one, 'hello' as greeting, TRUE as flag";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "one").nullable);
    assert!(!col(&info, "greeting").nullable);
    assert!(!col(&info, "flag").nullable);
}

#[test]
fn stress_select_null_literal() {
    let db = setup();
    let sql = "SELECT NULL as nothing";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "nothing").nullable, "NULL literal is nullable");
}

#[test]
fn stress_ambiguous_id_columns() {
    let db = setup();
    // Both tables have 'id' — must use aliases to disambiguate.
    let sql = "SELECT u.id as user_id, p.id as post_id \
               FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "user_id").nullable);
    assert!(!col(&info, "post_id").nullable);
}

#[test]
fn err_ambiguous_column_lists_candidate_tables() {
    let db = setup();
    // Unqualified `id` is present in both `users` and `posts` — the error
    // must keep PG's prefix and add a suffix listing every alias that
    // could provide it, in FROM-clause order.
    let sql = "SELECT id FROM users u INNER JOIN posts p ON p.user_id = u.id";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::AmbiguousColumn(_),
        concat!(
            "column reference \"id\" is ambiguous (could be: u.id, p.id)\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users u INNER JOIN posts p ON p.user_id = u.id\n",
            "  ·        ─┬\n",
            "  ·         ╰─ ambiguous reference\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn err_ambiguous_column_lists_three_candidates() {
    let db = setup();
    // Three tables all expose `id` — confirm every alias shows up.
    let sql = "SELECT id FROM users u \
               INNER JOIN posts p ON p.user_id = u.id \
               INNER JOIN comments c ON c.post_id = p.id";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::AmbiguousColumn(_),
        concat!(
            "column reference \"id\" is ambiguous (could be: u.id, p.id, c.id)\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users u INNER JOIN posts p ON p.user_id = u.id INNER JOIN comments c ON c.post_id = p.id\n",
            "  ·        ─┬\n",
            "  ·         ╰─ ambiguous reference\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn stress_distinct_on() {
    let db = setup();
    // Differs from `distinct_on` by adding the nullable `body` column.
    let sql = "SELECT DISTINCT ON (user_id) user_id, title, body \
               FROM posts ORDER BY user_id, published_at DESC NULLS LAST";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![c("user_id", int8()), c("title", text()), cn("body", text())],
    );
}

// ── Mixed computed and direct columns ────────────────────────────────────────

#[test]
fn complex_mixed_computed_and_direct_cols() {
    let db = setup();
    let sql = "SELECT \
                   u.id, \
                   u.name, \
                   u.age, \
                   COUNT(*) as post_count, \
                   COALESCE(u.age, 0) as safe_age, \
                   u.name || ' <' || u.email || '>' as display \
               FROM users u \
               INNER JOIN posts p ON p.user_id = u.id \
               GROUP BY u.id, u.name, u.age, u.email";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            c("name", text()),
            cn("age", int4()),
            // COUNT(*): NOT NULL int8.
            c("post_count", int8()),
            // COALESCE(age, 0): NOT NULL int4.
            c("safe_age", int4()),
            // String concat with NOT NULL cols: NOT NULL text.
            c("display", text()),
        ],
    );
}

// ── TABLESAMPLE ─────────────────────────────────────────────────────────────
//
// `TABLESAMPLE BERNOULLI(p)` / `TABLESAMPLE SYSTEM(p)` is a FROM-clause
// modifier — it doesn't change the projected columns or their nullability.
// PG accepts it; the analyzer should too.

#[test]
fn tablesample_bernoulli_preserves_columns() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users TABLESAMPLE BERNOULLI(50)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn tablesample_system_with_repeatable() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users TABLESAMPLE SYSTEM(10) REPEATABLE(42)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn tablesample_with_alias_preserves_columns() {
    let db = setup();
    // Per PG syntax the alias goes on the relation, then TABLESAMPLE.
    let s = db
        .analyze("SELECT u.id, u.name FROM users AS u TABLESAMPLE BERNOULLI(50)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn tablesample_with_join_resolves_other_table() {
    let db = setup();
    // TABLESAMPLE on one side of a join doesn't affect the other side.
    let s = db
        .analyze(
            "SELECT u.id, p.title \
             FROM users AS u TABLESAMPLE BERNOULLI(10) \
             JOIN posts p ON p.user_id = u.id",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("title", text())]);
}

// ── FOR UPDATE / FOR SHARE ──────────────────────────────────────────────────
//
// Locking clauses don't affect the row shape — projections come back
// exactly the same. The analyzer must accept them and not reject the
// query.

#[test]
fn select_for_update_preserves_columns() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users WHERE id = $p1 FOR UPDATE")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn select_for_share_preserves_columns() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE id = $p1 FOR SHARE")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn select_for_update_skip_locked() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE id = $p1 FOR UPDATE SKIP LOCKED")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn select_for_update_nowait() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE id = $p1 FOR UPDATE NOWAIT")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn select_for_update_of_specific_table() {
    let db = setup();
    // `FOR UPDATE OF u` — only locks the named table in a join.
    let s = db
        .analyze(
            "SELECT u.id, p.title FROM users u JOIN posts p ON p.user_id = u.id \
             FOR UPDATE OF u",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("title", text())]);
}

// ── Inherited tables — `pg_inherits` is modeled ─────────────────────────────
//
// `SELECT FROM child` resolves columns merged in from each parent, in
// addition to the child's own.

#[test]
fn select_from_child_table_sees_inherited_columns() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE animals (
            name  TEXT NOT NULL,
            sound TEXT NOT NULL
         );
         CREATE TABLE dogs (
            breed TEXT NOT NULL
         ) INHERITS (animals);",
    )
    .unwrap();

    // PG: `name` and `sound` are visible on `dogs` via inheritance.
    let s = db.analyze("SELECT name, sound, breed FROM dogs").unwrap();
    assert_cols(
        &s,
        vec![c("name", text()), c("sound", text()), c("breed", text())],
    );
}

// ── ORDER BY column validation + select-alias fallback ───────────────────────

#[test]
fn order_by_unknown_column_rejected() {
    // A typo in ORDER BY used to pass silently — the walker discarded
    // `infer_expr`'s error. Now it propagates, with a fallback for
    // select aliases (see `order_by_resolves_select_alias`).
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users ORDER BY ghost"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users ORDER BY ghost\n",
            "  ·                               ──┬──\n",
            "  ·                                 ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn order_by_resolves_select_alias() {
    // PG accepts `ORDER BY <select_alias>` even though the alias isn't
    // in the FROM scope. The fallback in the sort_clause walk keeps
    // this working after the propagation fix.
    let db = setup();
    let s = db
        .analyze("SELECT name AS author FROM users ORDER BY author")
        .unwrap();
    assert_cols(&s, vec![c("author", text())]);
}

#[test]
fn order_by_complex_expression_with_unknown_column_rejected() {
    // The alias fallback only applies to BARE column refs — a typo
    // inside a wider expression (e.g. `ORDER BY ghost + 1`) must still
    // surface as an error.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT name AS ghost FROM users ORDER BY ghost + 1"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT name AS ghost FROM users ORDER BY ghost + 1\n",
            "  ·                                          ──┬──\n",
            "  ·                                            ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

// ── UndefinedTable: full rendered diagnostic ────────────────────────────────
//
// The analyzer renders `UndefinedTable` with a `rustc`-style snippet that
// pinpoints the offending token in the SQL and (when possible) suggests a
// closely-named relation. The first line is the PostgreSQL-verbatim
// message — `pg_sanity`'s prefix check still passes — and the rest is
// extra context.

#[test]
fn undefined_table_in_from_renders_snippet_and_hint() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM userz WHERE id = 1"),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"userz\" does not exist
  ╭────
1 │ SELECT id FROM userz WHERE id = 1
  ·                ──┬──
  ·                  ╰─ relation does not exist
  ╰────
  help: did you mean \"users\"?\n",
    );
}

#[test]
fn undefined_table_with_no_close_match_omits_hint() {
    // `xyzabc` is too distant from any visible relation — no `did you mean`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM xyzabc"),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"xyzabc\" does not exist
  ╭────
1 │ SELECT id FROM xyzabc
  ·                ───┬──
  ·                   ╰─ relation does not exist
  ╰────\n",
    );
}

#[test]
fn undefined_table_schema_qualified_caret_covers_full_name() {
    // `public.userz` — caret underlines the qualified form (12 chars), not
    // just the relation name. `at_qualified_name` walks the `schema.name`
    // form when the AST `location` points at the start of `public`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM public.userz"),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"public.userz\" does not exist
  ╭────
1 │ SELECT id FROM public.userz
  ·                ──────┬─────
  ·                      ╰─ relation does not exist
  ╰────
  help: did you mean \"users\"?\n",
    );
}

#[test]
fn undefined_table_multiline_sql_locates_offending_line() {
    // Multi-line SQL: line/column point at the correct line, snippet shows
    // only that line, gutter widens to the largest displayed line number.
    let db = setup();
    let sql = "SELECT id\nFROM userz\nWHERE id = 1";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"userz\" does not exist
  ╭────
2 │ FROM userz
  ·      ──┬──
  ·        ╰─ relation does not exist
  ╰────
  help: did you mean \"users\"?\n",
    );
}

#[test]
fn undefined_table_with_named_param_keeps_original_offsets() {
    // The lexer rewrites `$id` to `$1` before parsing, but the diagnostic
    // points at the relation in the ORIGINAL SQL — so the column stays
    // aligned with what the user wrote, not with the rewritten form.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT * FROM userz WHERE id = $id"),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"userz\" does not exist
  ╭────
1 │ SELECT * FROM userz WHERE id = $id
  ·               ──┬──
  ·                 ╰─ relation does not exist
  ╰────
  help: did you mean \"users\"?\n",
    );
}

#[test]
fn undefined_table_in_join_locates_second_relation() {
    // The first relation exists; only the JOIN's right side is bad. The
    // caret must point at the second relation, not the first.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT u.id FROM users u JOIN postz p ON p.user_id = u.id"),
        AnalyzeError::UndefinedTable(_),
        "\
relation \"postz\" does not exist
  ╭────
1 │ SELECT u.id FROM users u JOIN postz p ON p.user_id = u.id
  ·                               ──┬──
  ·                                 ╰─ relation does not exist
  ╰────
  help: did you mean \"posts\"?\n",
    );
}

// ── UndefinedColumn: full rendered diagnostic ──────────────────────────────

#[test]
fn undefined_column_bare_with_hint() {
    // `nme` is one edit away from `name` — the snippet should point at the
    // typo and offer a hint.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT nme FROM users"),
        AnalyzeError::UndefinedColumn(_),
        "\
column \"nme\" does not exist
  ╭────
1 │ SELECT nme FROM users
  ·        ─┬─
  ·         ╰─ column does not exist
  ╰────
  help: did you mean \"name\"?
",
    );
}

#[test]
fn undefined_column_qualified_caret_covers_full_ref() {
    // `u.nme` — caret covers the qualified column reference.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT u.nme FROM users u"),
        AnalyzeError::UndefinedColumn(_),
        "\
column u.nme does not exist
  ╭────
1 │ SELECT u.nme FROM users u
  ·        ──┬──
  ·          ╰─ column does not exist
  ╰────
  help: did you mean \"name\"?
",
    );
}

#[test]
fn undefined_column_in_where_locates_correct_token() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE ema = 'x'"),
        AnalyzeError::UndefinedColumn(_),
        "\
column \"ema\" does not exist
  ╭────
1 │ SELECT id FROM users WHERE ema = 'x'
  ·                            ─┬─
  ·                             ╰─ column does not exist
  ╰────
  help: did you mean \"email\"?
",
    );
}

// ── UndefinedFunction: full rendered diagnostic ────────────────────────────

#[test]
fn undefined_function_with_hint() {
    // `lengt(name)` — analyzer's first-pass overload lookup finds no name
    // match at all, so the wording stays in the `function name() does not
    // exist` form (no arg-types interpolation). The caret + hint still
    // point at the typo.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT lengt(name) FROM users"),
        AnalyzeError::UndefinedFunction(_),
        "\
function lengt(text) does not exist
  ╭────
1 │ SELECT lengt(name) FROM users
  ·        ──┬──
  ·          ╰─ function does not exist
  ╰────
  help: did you mean \"length\"?
",
    );
}

#[test]
fn undefined_function_schema_qualified() {
    // `pg_catalog.nonexisting()` — caret covers the qualified name.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT pg_catalog.nonexisting() FROM users"),
        AnalyzeError::UndefinedFunction(_),
        "\
function pg_catalog.nonexisting() does not exist
  ╭────
1 │ SELECT pg_catalog.nonexisting() FROM users
  ·        ───────────┬──────────
  ·                   ╰─ function does not exist
  ╰────
",
    );
}

#[test]
fn undefined_column_ambiguous_lists_candidates() {
    // `id` exists in both `users` and `posts` — the diagnostic lists both
    // qualified candidates in the message body, carried by the dedicated
    // `AmbiguousColumn` variant (SQLSTATE 42702, like PG).
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users u JOIN posts p ON p.user_id = u.id"),
        AnalyzeError::AmbiguousColumn(_),
        "\
column reference \"id\" is ambiguous (could be: u.id, p.id)
  ╭────
1 │ SELECT id FROM users u JOIN posts p ON p.user_id = u.id
  ·        ─┬
  ·         ╰─ ambiguous reference
  ╰────
",
    );
}

// ── Implicit output-column naming (PG FigureColname) ────────────────────────

#[test]
fn cast_of_column_keeps_column_name() {
    let db = setup();
    // PG names a cast after its argument when the argument has a strong name,
    // so `age::text` (no alias) is named `age`, not `text`.
    let s = db.analyze("SELECT age::text FROM users").unwrap();
    assert_cols(&s, vec![cn("age", text())]);
}

#[test]
fn cast_of_literal_falls_back_to_type_name() {
    let db = setup();
    // A constant has no strong name, so the cast falls back to the target
    // type's name: `1::int4` is named `int4`.
    let s = db.analyze("SELECT 1::int4").unwrap();
    assert_cols(&s, vec![c("int4", int4())]);
}

#[test]
fn cast_of_function_call_keeps_function_name() {
    let db = setup();
    // A function call is a strong name, so it survives the cast: `upper(name)
    // ::text` is named `upper`.
    let s = db.analyze("SELECT upper(name)::text FROM users").unwrap();
    assert_cols(&s, vec![c("upper", text())]);
}

// ── Lex errors: snippet points at the unclosed token start ─────────────────

#[test]
fn lex_unclosed_string_locates_opening_quote() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name = 'oops"),
        AnalyzeError::Lex(_),
        "\
unterminated quoted string at or near \"'oops\"
  ╭────
1 │ SELECT id FROM users WHERE name = 'oops
  ·                                   ─
  ╰────
",
    );
}

#[test]
fn lex_unclosed_block_comment_locates_opening_slash_star() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT 1 /* unterminated"),
        AnalyzeError::Lex(_),
        "\
unterminated /* comment at or near \"/* unterminated\"
  ╭────
1 │ SELECT 1 /* unterminated
  ·          ─
  ╰────
",
    );
}

#[test]
fn unqualified_system_columns_resolve_on_tables_only() {
    // PG 18 colNameToVar / scanRTEForColumn: a bare system column name
    // resolves against every relation RTE (ambiguous across two); views
    // have no system attributes, and neither do subqueries.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (a int);
         CREATE TABLE u (b int);
         CREATE VIEW v AS SELECT * FROM t;",
    )
    .unwrap();
    for sql in [
        "SELECT ctid FROM t",
        "SELECT a FROM t WHERE xmin = '1'",
        "SELECT u.tableoid, t.cmin FROM t JOIN u ON b = a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (sql, msg) in [
        (
            "SELECT ctid FROM t, u",
            "column reference \"ctid\" is ambiguous",
        ),
        ("SELECT v.ctid FROM v", "column v.ctid does not exist"),
        ("SELECT ctid FROM v", "column \"ctid\" does not exist"),
        (
            "SELECT tableoid FROM (SELECT * FROM t) s",
            "column \"tableoid\" does not exist",
        ),
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
}

#[test]
fn qualified_star_expands_any_visible_entry() {
    // PG's ExpandColumnRefStar resolves `t.*` like any qualifier: a LATERAL
    // or outer entry works, `schema.t.*` names the table, and an unknown
    // name is a missing FROM-clause entry, not an empty expansion.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE src (id int PRIMARY KEY, name text NOT NULL, amount int);")
        .unwrap();
    let s = db
        .analyze("SELECT * FROM src s2, LATERAL (SELECT s2.*) l")
        .unwrap();
    assert_eq!(s.columns.len(), 6);
    let s = db.analyze("SELECT public.src.* FROM src").unwrap();
    assert_cols(
        &s,
        vec![c("id", int4()), c("name", text()), cn("amount", int4())],
    );
    let err = db.analyze("SELECT nosuch.* FROM src").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("missing FROM-clause entry for table \"nosuch\""),
        "{err}"
    );
}

/// A database-qualified name (`db.schema.table.column`, `db.schema.type`,
/// `db.schema.function`) is a cross-database reference unless `db` is the
/// current database — which the analyzer cannot know, so it takes every
/// qualifier as another database; more dotted names never resolve.
#[test]
fn database_qualified_names_are_cross_database_references() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT x.public.users.id FROM users",
            "cross-database references are not implemented: x.public.users.id",
        ),
        (
            "SELECT x.public.users.* FROM users",
            "cross-database references are not implemented: x.public.users.*",
        ),
        (
            "SELECT 1::x.public.int4",
            "cross-database references are not implemented: x.public.int4",
        ),
        (
            "SELECT x.public.lower('a')",
            "cross-database references are not implemented: x.public.lower",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::FeatureNotSupported(_), msg);
    }
    assert_err_prefix!(
        db.analyze("SELECT a.b.c.d.e FROM users"),
        AnalyzeError::SyntaxError(_),
        "improper qualified name (too many dotted names): a.b.c.d.e"
    );
}

/// A database-qualified relation (`db.schema.rel`) in FROM or as a DML
/// target is a cross-database reference like any other catalog qualifier.
#[test]
fn database_qualified_relations_are_cross_database_references() {
    let db = setup();
    for sql in [
        "SELECT * FROM x.public.users",
        "SELECT u.id FROM posts JOIN x.public.users u ON u.id = posts.user_id",
        "INSERT INTO x.public.users (id) VALUES (1)",
        "UPDATE x.public.users SET age = 1",
        "DELETE FROM x.public.users",
        "MERGE INTO x.public.users u USING posts p ON u.id = p.user_id WHEN MATCHED THEN DELETE",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::FeatureNotSupported(_),
            "cross-database references are not implemented: \"x.public.users\""
        );
    }
}
