//! Less-common query constructs: LATERAL, VALUES, set-returning functions
//! (SRFs), `(expr).field` indirection, `arr[i]`, `row_to_json`,
//! UNKNOWN-literal resolution in operators/functions, subquery alias
//! overrides.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id          BIGINT PRIMARY KEY,
            name        TEXT NOT NULL,
            age         INT,
            preferences JSONB
        );
         CREATE TABLE comments (
            id          BIGINT PRIMARY KEY,
            post_id     BIGINT NOT NULL,
            author_name TEXT NOT NULL
         );
         CREATE SCHEMA whatsapp;
         CREATE TABLE whatsapp.contacts (
            channel_id  BIGINT NOT NULL,
            id          TEXT NOT NULL,
            name        TEXT,
            pushname    TEXT,
            is_business BOOLEAN,
            PRIMARY KEY (channel_id, id)
         );",
    )
    .unwrap();
    db
}

// ── UNKNOWN literals in function calls ───────────────────────────────────────

#[test]
fn unknown_literal_in_function_call() {
    let db = setup();
    // ', ' is UNKNOWN — should resolve `string_agg(text, text)` unambiguously.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "authors").pg_type, text());
}

#[test]
fn unknown_literal_in_replace() {
    let db = setup();
    // replace(text, text, text) — two UNKNOWN literals.
    let sql = "SELECT replace(name, 'foo', 'bar') as replaced FROM users";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "replaced").pg_type, text());
    assert!(!col(&info, "replaced").nullable);
}

#[test]
fn unknown_literal_in_position() {
    let db = setup();
    let sql = "SELECT position('x' in name) as pos FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "pos").nullable);
}

// ── UNKNOWN type resolution in operators ─────────────────────────────────────

/// jsonb `?` operator with both sides UNKNOWN resolves to `jsonb ? text → bool`
/// (unique candidate), which then types the param as jsonb.
#[test]
fn unknown_operator_jsonb_exists() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE preferences ? 'theme'";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "id").pg_type, int8());
}

/// jsonb `->>` with a typed left side and UNKNOWN right resolves to
/// `jsonb ->> text → text` (text-fallback disambiguation).
#[test]
fn unknown_operator_jsonb_arrow_text() {
    let db = setup();
    let sql = "SELECT preferences->>'theme' as theme FROM users";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "theme").pg_type, text());
    assert!(col(&info, "theme").nullable);
}

/// Param used with `?` first (infers jsonb), then with `->>` — the second
/// usage should see the already-inferred type.
#[test]
fn unknown_param_jsonb_exists_then_arrow() {
    let db = setup();
    let sql = "UPDATE whatsapp.contacts SET \
               name = CASE WHEN $p1 ? 'name' THEN $p1->>'name' ELSE name END \
               WHERE channel_id = $p2 AND id = $p3";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params[0].pg_type, jsonb());
    assert_eq!(info.params[1].pg_type, int8());
    assert_eq!(info.params[2].pg_type, text());
}

/// Multiple CASE WHEN branches using `?` and `->>` with the same param.
#[test]
fn unknown_param_jsonb_multiple_case_branches() {
    let db = setup();
    let sql = "UPDATE whatsapp.contacts SET \
               name = CASE WHEN $p1 ? 'name' THEN $p1->>'name' ELSE name END, \
               pushname = CASE WHEN $p1 ? 'pushname' THEN $p1->>'pushname' ELSE pushname END, \
               is_business = CASE WHEN $p1 ? 'is_business' THEN ($p1->>'is_business')::boolean ELSE is_business END \
               WHERE channel_id = $p2 AND id = $p3";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params[0].pg_type, jsonb());
    assert_eq!(info.params[1].pg_type, int8());
    assert_eq!(info.params[2].pg_type, text());
}

/// Operator `->` (returns jsonb) with UNKNOWN right side should resolve.
#[test]
fn unknown_operator_jsonb_arrow() {
    let db = setup();
    let sql = "SELECT preferences->'theme' as theme FROM users";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "theme").pg_type, jsonb());
}

/// Query against pg_catalog table: obj_description function + name column.
#[test]
fn pg_catalog_obj_description_with_param() {
    let db = setup();
    let sql = "SELECT obj_description(oid) as comment FROM pg_namespace WHERE nspname = $p1";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "comment").pg_type, text());
    assert!(col(&info, "comment").nullable);
    assert_eq!(info.params[0].pg_type, name_ty());
}

// ── AIndirection — `(expr).field` ────────────────────────────────────────────

#[test]
fn indirection_field_on_composite_column() {
    // A table's composite type is registered under the table's QN.
    // `(u).name` takes the composite row and pulls a named field out.
    let db = setup();
    let sql = "SELECT (u).name AS the_name FROM users u";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "the_name").pg_type, text());
}

#[test]
fn indirection_field_nullability_honors_field_not_null() {
    let db = setup();
    // `users.age` is nullable, so `(u).age` must be nullable too.
    let sql = "SELECT (u).age AS the_age FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "the_age").nullable);
}

#[test]
fn indirection_field_unknown_errors() {
    // PG (SQLSTATE 42703) reports `column u.nao_existe does not exist` for
    // the bare-relation-alias indirection shape `(alias).field` — distinct
    // from the chained / composite-column wording (`column "f" not found
    // in data type T`). The analyzer mirrors PG's exact message, so no
    // mirror skip is needed.
    let db = setup();
    let sql = "SELECT (u).nao_existe FROM users u";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::UndefinedColumn(_),
        "column u.nao_existe does not exist",
    );
}

// ── ARRAY(SELECT …) sublink ──────────────────────────────────────────────────

#[test]
fn array_sublink_of_text_returns_text_array() {
    let db = setup();
    let sql = "SELECT ARRAY(SELECT name FROM users) AS names";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "names").pg_type, array_with_elems(text(), false));
    // ARRAY() always returns a non-null array (empty if no rows).
    assert!(!col(&info, "names").nullable);
}

#[test]
fn array_sublink_of_int4_returns_int4_array() {
    let db = setup();
    let sql = "SELECT ARRAY(SELECT age FROM users WHERE age IS NOT NULL) AS ages";
    let info = db.analyze(sql).unwrap();
    // The WHERE does not narrow the column: its elements stay NULL-able.
    assert_eq!(col(&info, "ages").pg_type, array_with_elems(int4(), true));
    assert!(!col(&info, "ages").nullable);
}

// ── `FROM func(...)` — RangeFunction ─────────────────────────────────────────

#[test]
fn range_function_scalar_srf_exposes_single_column() {
    let db = setup();
    let sql = "SELECT generate_series FROM generate_series(1, 10)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "generate_series").pg_type, int4());
}

#[test]
fn range_function_argument_errors_are_not_swallowed() {
    // LATERAL or not, an argument that doesn't transform aborts the query —
    // it must not degrade to an `unknown` argument.
    let db = setup();
    for sql in [
        "SELECT g FROM users u, LATERAL generate_series(1, u.nope) AS g",
        "SELECT g FROM users u, LATERAL unnest(u.nope) AS g",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            err.to_string().starts_with("column u.nope does not exist"),
            "{sql}: {err}"
        );
    }
}

#[test]
fn range_function_scalar_column_takes_the_relation_alias() {
    let db = setup();
    let info = db
        .analyze("SELECT g, g.g AS qualified FROM generate_series(1, 10) AS g")
        .unwrap();
    assert_cols(&info, vec![c("g", int4()), c("qualified", int4())]);
    let info = db
        .analyze("SELECT * FROM unnest(ARRAY[1]) WITH ORDINALITY AS u")
        .unwrap();
    assert_cols(&info, vec![c("u", int4()), c("ordinality", int8())]);
    // A composite result keeps its field names.
    let info = db
        .analyze("SELECT * FROM pg_options_to_table(ARRAY['a=b']) AS o")
        .unwrap();
    assert_eq!(col(&info, "option_name").pg_type, text());
}

#[test]
fn range_function_with_out_args_exposes_named_columns() {
    let db = setup();
    // pg_options_to_table(text[]) is declared with
    // TABLE(option_name text, option_value text).
    let sql = "SELECT option_name, option_value \
               FROM pg_options_to_table(ARRAY['a=b', 'c=d']::text[])";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "option_name").pg_type, text());
    assert_eq!(col(&info, "option_value").pg_type, text());
}

#[test]
fn range_function_column_alias_list_overrides_names() {
    let db = setup();
    let sql = "SELECT n FROM generate_series(1, 5) AS t(n)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "n").pg_type, int4());
}

#[test]
fn range_function_with_ordinality_appends_bigint_column() {
    let db = setup();
    let sql = "SELECT n, ord FROM generate_series(1, 3) WITH ORDINALITY AS t(n, ord)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "n").pg_type, int4());
    assert_eq!(col(&info, "ord").pg_type, int8());
    assert!(!col(&info, "ord").nullable);
}

// ── (func(...)).field — indirection over a SRF with out_args ─────────────────

#[test]
fn indirection_on_srf_with_out_args_resolves_named_field() {
    let db = setup();
    let sql = "SELECT (pg_options_to_table(ARRAY['a=b']::text[])).option_name AS opt";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "opt").pg_type, text());
}

#[test]
fn indirection_on_srf_unknown_field_errors() {
    let db = setup();
    let sql = "SELECT (pg_options_to_table(ARRAY['a=b']::text[])).nao_existe";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::UndefinedColumn(_),
        "could not identify column \"nao_existe\" in record data type",
    );
}

// ── Record field via subquery column (record_fields propagation) ─────────────

#[test]
fn indirection_on_subquery_record_column() {
    // _pg_expandarray returns a record with named out_args. When held in a
    // subquery column and referenced via `(ta.x).n`, the analyzer must resolve
    // through the record_fields carried on the scope column.
    let db = setup();
    let sql = "SELECT (ta.x).n AS idx \
               FROM (SELECT information_schema._pg_expandarray(ARRAY[1, 2]) AS x) ta";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "idx").pg_type, int4());
}

// ── VALUES (…) in FROM with column alias list ────────────────────────────────

#[test]
fn values_in_from_with_column_alias_list() {
    let db = setup();
    let sql = "SELECT em.num, em.text \
               FROM (VALUES (4, 'INSERT'::text), (8, 'DELETE'::text)) AS em(num, text)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "num").pg_type, int4());
    assert_eq!(col(&info, "text").pg_type, text());
}

// ── LATERAL — inner subquery sees outer FROM scope ───────────────────────────

#[test]
fn lateral_subquery_sees_outer_scope() {
    let db = setup();
    let sql = "SELECT u.name, s.double_id \
               FROM users u, LATERAL (SELECT u.id * 2 AS double_id) s";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "name").pg_type, text());
    assert_eq!(col(&info, "double_id").pg_type, int8());
}

#[test]
fn lateral_subquery_multi_column() {
    let db = setup();
    // Multiple projected columns in the LATERAL body all inherit `u`'s
    // scope and carry distinct types out.
    let sql = "SELECT u.id, s.a, s.b \
               FROM users u, LATERAL (SELECT u.id + 1 AS a, u.name AS b) s";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "id").pg_type, int8());
    assert_eq!(col(&info, "a").pg_type, int8());
    assert_eq!(col(&info, "b").pg_type, text());
    assert!(!col(&info, "a").nullable);
    assert!(!col(&info, "b").nullable);
}

#[test]
fn lateral_subquery_with_column_alias_list() {
    let db = setup();
    // `AS s(x, y)` after a LATERAL subquery renames the projected columns
    // positionally — same shape as the non-LATERAL path.
    let sql = "SELECT u.id, s.x, s.y \
               FROM users u, LATERAL (SELECT u.id, u.name) AS s(x, y)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "x").pg_type, int8());
    assert_eq!(col(&info, "y").pg_type, text());
}

#[test]
fn lateral_subquery_with_param() {
    let db = setup();
    // A param inside the LATERAL body is typed from the outer arithmetic
    // context; `u.id + $p1` pins `$p1` to int8.
    let sql = "SELECT u.id, s.val \
               FROM users u, LATERAL (SELECT u.id + $p1 AS val) s";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "val").pg_type, int8());
    assert_eq!(info.params.len(), 1);
    assert_eq!(info.params[0].pg_type, int8());
}

#[test]
fn lateral_subquery_with_row_to_json() {
    let db = setup();
    // `row_to_json(u.*)` inside LATERAL sees the composite row of `u` via
    // the inherited scope — result is a non-null json.
    let sql = "SELECT u.id, j.doc \
               FROM users u, LATERAL (SELECT row_to_json(u.*) AS doc) j";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "doc").pg_type, json_ty());
    assert!(!col(&info, "doc").nullable);
}

// ── LATERAL with JOIN — table alias sees rows on the left ────────────────────

#[test]
fn cross_join_lateral_with_limit_1() {
    let db = setup();
    // Classic "for each user, fetch their top comment author" — the inner
    // SELECT is correlated and LIMIT 1 makes it produce at most one row per
    // outer row.
    let sql = "SELECT u.id, top_c.author_name \
               FROM users u \
               CROSS JOIN LATERAL ( \
                   SELECT c.author_name FROM comments c \
                   WHERE c.post_id = u.id \
                   ORDER BY c.id DESC LIMIT 1 \
               ) top_c";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "id").pg_type, int8());
    assert_eq!(col(&info, "author_name").pg_type, text());
    assert!(!col(&info, "author_name").nullable);
}

#[test]
fn left_join_lateral_produces_nullable_columns() {
    let db = setup();
    // LEFT JOIN LATERAL: if the inner SELECT yields zero rows for a given
    // outer row, those columns come back NULL — so the join reporter must
    // mark them nullable even when the inner `author_name` is NOT NULL.
    let sql = "SELECT u.id, top_c.author_name \
               FROM users u \
               LEFT JOIN LATERAL ( \
                   SELECT c.author_name FROM comments c \
                   WHERE c.post_id = u.id \
                   ORDER BY c.id DESC LIMIT 1 \
               ) top_c ON true";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "id").pg_type, int8());
    assert_eq!(col(&info, "author_name").pg_type, text());
    assert!(col(&info, "author_name").nullable);
}

#[test]
fn lateral_correlated_aggregate() {
    let db = setup();
    // COUNT(*) in a correlated LATERAL: every outer row still gets a row
    // back (COUNT returns 0 for empty input), so the result is NOT NULL.
    let sql = "SELECT u.id, agg.total \
               FROM users u \
               CROSS JOIN LATERAL ( \
                   SELECT COUNT(*) AS total FROM comments c WHERE c.post_id = u.id \
               ) agg";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "total").pg_type, int8());
    assert!(!col(&info, "total").nullable);
}

#[test]
fn left_join_lateral_aggregate_becomes_nullable() {
    let db = setup();
    // Semantically COUNT(*) always produces a row, so `total` should stay
    // NOT NULL even under LEFT JOIN LATERAL. The analyzer conservatively
    // treats every LEFT JOIN'd column as nullable because it doesn't
    // reason about "this subquery is guaranteed non-empty". That matches
    // PG's own planner output (columns from a LEFT JOIN'd relation are
    // reported as nullable in ROW descriptions), so we freeze the
    // conservative behavior here.
    let sql = "SELECT u.id, agg.total \
               FROM users u \
               LEFT JOIN LATERAL ( \
                   SELECT COUNT(*) AS total FROM comments c WHERE c.post_id = u.id \
               ) agg ON true";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "total").nullable);
}

// ── LATERAL on an SRF (RangeFunction) ────────────────────────────────────────

#[test]
fn lateral_unnest_correlated_array_built_from_outer_column() {
    let db = setup();
    // `ARRAY[u.name, 'extra']` is an outer-correlated array expression
    // whose element type (`text`) must reach the SRF. Without the LATERAL
    // scope wiring for RangeFunction args, `u.name` would resolve to
    // UNKNOWN and `unnest` would pick the wrong polymorphic overload
    // (`anymultirange -> anyrange`).
    //
    // Both elements (`u.name` text NOT NULL + the literal `'extra'`) are
    // non-null, so the unnested column inherits NOT NULL — the analyzer
    // is allowed to be more precise than PG's RowDescription, which
    // would conservatively report nullable.
    let sql = "SELECT u.id, t.tag \
               FROM users u, LATERAL unnest(ARRAY[u.name, 'extra']) AS t(tag)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "id").pg_type, int8());
    assert_eq!(col(&info, "tag").pg_type, text());
    assert!(!col(&info, "tag").nullable);
}

// ── Nested LATERAL ───────────────────────────────────────────────────────────

#[test]
fn nested_lateral_inner_sees_outermost_scope() {
    let db = setup();
    // Inner LATERAL sees both its immediate parent (the middle LATERAL)
    // and the outermost `users u`. The chain of scope inheritance has to
    // reach all the way out.
    let sql = "SELECT u.id, outer_s.inner_val \
               FROM users u, LATERAL ( \
                   SELECT inner_s.v AS inner_val FROM LATERAL (SELECT u.id * 3 AS v) inner_s \
               ) outer_s";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "inner_val").pg_type, int8());
}

// ── Plain (non-LATERAL) FROM does NOT inherit scope ──────────────────────────

#[test]
fn function_in_from_can_see_outer_scope_implicitly() {
    let db = setup();
    // PG: `LATERAL` is a noise word in front of a function-call FROM item —
    // the args can reference earlier FROM items either way. So
    // `FROM users u, generate_series(1, u.id) g(n)` resolves cleanly even
    // without an explicit `LATERAL` keyword. Since `u.id` is int8, PG
    // picks the `generate_series(int8, int8) -> int8` overload.
    let info = db
        .analyze("SELECT u.id, g.n FROM users u, generate_series(1, u.id) g(n)")
        .unwrap();
    assert_eq!(col(&info, "id").pg_type, int8());
    assert_eq!(col(&info, "n").pg_type, int8());
}

#[test]
fn non_lateral_subquery_cannot_see_outer_scope() {
    // Without LATERAL the inner subquery can't reference `u.id`. PG emits
    // `invalid reference to FROM-clause entry for table "u"`. The analyzer
    // mirrors that wording via the `shadowed_sources` channel populated
    // when descending into a non-LATERAL FROM subquery — no skip needed.
    let db = setup();
    let sql = "SELECT u.name, s.double_id \
               FROM users u, (SELECT u.id * 2 AS double_id) s";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::UndefinedTable(_),
        concat!(
            "invalid reference to FROM-clause entry for table \"u\"\n",
            "  ╭────\n",
            "1 │ SELECT u.name, s.double_id FROM users u, (SELECT u.id * 2 AS double_id) s\n",
            "  ·                                                  ────\n",
            "  ╰────\n",
        ),
    );
}

// ── Subquery column alias list overrides inner names ─────────────────────────

#[test]
fn subquery_column_alias_list_overrides_inner_names() {
    let db = setup();
    let sql = "SELECT x.user_id, x.user_name \
               FROM (SELECT id, name FROM users) AS x(user_id, user_name)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "user_id").pg_type, int8());
    assert_eq!(col(&info, "user_name").pg_type, text());
}

// ── EXISTS without FROM ──────────────────────────────────────────────────────

#[test]
fn exists_without_from() {
    let db = setup();
    // SELECT EXISTS(...) without FROM — always returns exactly 1 row, never NULL.
    let sql = "SELECT EXISTS(SELECT 1 FROM users WHERE id = $p1)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.columns.len(), 1);
    assert!(!info.columns[0].nullable, "EXISTS should be NOT NULL");
    assert_eq!(info.columns[0].pg_type, bool_ty());
}

#[test]
fn exists_constant_without_from() {
    let db = setup();
    // SELECT EXISTS(SELECT 1) — pure constant, no table reference at all.
    let sql = "SELECT EXISTS(SELECT 1)";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.columns.len(), 1);
    assert!(!info.columns[0].nullable, "EXISTS should be NOT NULL");
    assert_eq!(info.columns[0].pg_type, bool_ty());
}

// ── Duplicate output aliases ─────────────────────────────────────────────────
//
// PG allows the same column name to appear twice in SELECT; the analyzer
// should surface both without collapsing them or erroring.

#[test]
fn duplicate_output_column_name() {
    let db = setup();
    let s = db.analyze("SELECT id, id FROM users").unwrap();
    // Both columns survive with identical name + type.
    assert_cols(&s, vec![c("id", int8()), c("id", int8())]);
}

#[test]
fn duplicate_explicit_alias() {
    let db = setup();
    let s = db.analyze("SELECT id AS x, name AS x FROM users").unwrap();
    assert_cols(&s, vec![c("x", int8()), c("x", text())]);
}

// ── SRF in the SELECT list ───────────────────────────────────────────────────

#[test]
fn srf_unnest_in_select_list_returns_single_column() {
    let db = setup();
    // `SELECT unnest(ARRAY[1,2,3])` — an SRF in the projection expands into
    // a single column using the function name as the alias.
    let s = db.analyze("SELECT unnest(ARRAY[1,2,3])").unwrap();
    assert_eq!(s.columns.len(), 1);
    assert_eq!(col(&s, "unnest").pg_type, int4());
}

#[test]
fn srf_generate_series_int() {
    let db = setup();
    let s = db.analyze("SELECT generate_series(1, 10) AS n").unwrap();
    assert_cols(&s, vec![c("n", int4())]);
}

// ── Annotations on LEFT JOIN ─────────────────────────────────────────────────

#[test]
fn stress_annotation_on_left_join_star() {
    // Force nullable LEFT JOIN column to NOT NULL via annotation.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL
         );
         CREATE TABLE posts (
            id      BIGINT PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL
         );",
    )
    .unwrap();
    let sql = "SELECT u.name, p.title as \"title!\" \
               FROM users u LEFT JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "title").nullable);
}

// ── Parse-error message fidelity ─────────────────────────────────────────────

#[test]
fn syntax_error_message_matches_pg_verbatim() {
    // `typedpg_pg_query::Error::Parse`'s Display prepends "Invalid statement: " to the
    // server-side wording. The error-message contract requires our message to
    // *start with* PG's verbatim text, so the analyzer must strip that wrapper.
    // PG: `syntax error at or near "true"` — `EXTRACT(<expr> FROM …)` only
    // accepts an identifier/string field, so a boolean literal is a syntax error.
    // The caret underlines the token PG names, at the position it reports.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT extract(true FROM age) FROM users"),
        AnalyzeError::Parse(_),
        "\
syntax error at or near \"true\"
  ╭────
1 │ SELECT extract(true FROM age) FROM users
  ·                ────
  ╰────
",
    );
}

#[test]
fn syntax_error_points_at_its_position() {
    // The position is mapped back through the lexer's `$name` rewrite, onto
    // the line the user wrote it on.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id, name\nFROM users\nWHERE id = $id AND = 1"),
        AnalyzeError::Parse(_),
        "\
syntax error at or near \"=\"
  ╭────
3 │ WHERE id = $id AND = 1
  ·                    ─
  ╰────
",
    );
    // `at end of input` points just past the last character.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE"),
        AnalyzeError::Parse(_),
        "\
syntax error at end of input
  ╭────
1 │ SELECT id FROM users WHERE
  ·                           ─
  ╰────
",
    );
}

#[test]
fn extract_unresolved_reports_pg_catalog_qualified_name() {
    // The `EXTRACT(field FROM x)` grammar maps to `pg_catalog.extract`, so when
    // no overload matches PG reports the schema-qualified name. The analyzer
    // must keep that qualifier so the error-message prefix matches PG.
    // PG: `function pg_catalog.extract(unknown, integer) does not exist`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT extract('year' FROM age) FROM users"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function pg_catalog.extract(unknown, integer) does not exist (found 6 candidate(s))\n",
            "  ╭────\n",
            "1 │ SELECT extract('year' FROM age) FROM users\n",
            "  ·        ───┬───\n",
            "  ·           ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: did you mean \"extract\"?\n",
        ),
    );
}

#[test]
fn undefined_function_names_are_joined_raw_like_pg() {
    // func_signature_string uses NameListToString: no quoting, no escaping,
    // even for names quote_identifier would quote.
    let db = setup();
    let err = db
        .analyze("SELECT public.\"Order\"(1), 1 FROM users")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("function public.Order(integer) does not exist"),
        "{err}"
    );
}

// ── SQL value functions (CURRENT_DATE / CURRENT_USER / …) ────────────────────

#[test]
fn sql_value_functions_infer_types() {
    // typedpg_pg_query emits these as `SQLValueFunction` (with a 0 result OID in the raw
    // tree); the analyzer used to reject every one with "expression node type
    // not supported". Each maps to a concrete, NOT-NULL type — except
    // CURRENT_SCHEMA, NULL when no search-path schema exists.
    let db = setup();
    let s = db
        .analyze(
            "SELECT current_date AS d, current_timestamp AS ts, current_time AS t, \
                    localtimestamp AS lts, localtime AS lt, current_user AS u, \
                    current_schema AS sch, current_catalog AS cat, session_user AS su",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("d", date()),
            c("ts", timestamptz()),
            c("t", basic("pg_catalog", "timetz")),
            c("lts", timestamp()),
            c("lt", time_ty()),
            c("u", name_ty()),
            cn("sch", name_ty()),
            c("cat", name_ty()),
            c("su", name_ty()),
        ],
    );
}

#[test]
fn sql_value_function_unaliased_column_name_matches_pg() {
    // PG's FigureColname names an unaliased SQL value function after its keyword
    // spelling — `RETURNING current_date` → column `current_date` (not `?column?`).
    let db = setup();
    let s = db
        .analyze("DELETE FROM users RETURNING current_date")
        .unwrap();
    assert_eq!(s.columns[0].name, "current_date");
    let s = db
        .analyze("UPDATE users SET age = 1 RETURNING current_timestamp")
        .unwrap();
    assert_eq!(s.columns[0].name, "current_timestamp");
}
