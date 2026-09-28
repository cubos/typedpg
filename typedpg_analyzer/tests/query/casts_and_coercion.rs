//! Type casts and coercion: explicit `CAST(…)` / `::type`, implicit
//! promotion (int4 → int8, numeric tower), common-type resolution in
//! CASE/COALESCE/UNION branches.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            name  TEXT NOT NULL,
            age   INT
         );",
    )
    .unwrap();
    db
}

// ── Explicit casts ───────────────────────────────────────────────────────────

#[test]
fn types_match_cast_int_to_text() {
    let db = setup();
    let s = db
        .analyze("SELECT age::text AS age_text FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("age_text", text())]);
}

#[test]
fn types_match_cast_bigint_to_int() {
    let db = setup();
    let s = db
        .analyze("SELECT id::int4 AS short_id FROM users")
        .unwrap();
    assert_cols(&s, vec![c("short_id", int4())]);
}

#[test]
fn types_match_cast_literal() {
    let db = setup();
    let s = db.analyze("SELECT '123'::int4 AS val").unwrap();
    assert_cols(&s, vec![c("val", int4())]);
}

// ── Cast preserves nullability ───────────────────────────────────────────────

#[test]
fn complex_cast_preserves_nullability() {
    let db = setup();
    // Casting a nullable column preserves nullability.
    let sql = "SELECT age::text as age_text, id::text as id_text FROM users";
    let info = db.analyze(sql).unwrap();
    // age is nullable → age::text is nullable.
    assert!(col(&info, "age_text").nullable);
    // id is NOT NULL → id::text is NOT NULL.
    assert!(!col(&info, "id_text").nullable);
}

// ── Numeric tower: int2 → int4 → int8 → numeric → float4 → float8 ────────────

#[test]
fn numeric_tower_int2_to_int8() {
    let db = setup();
    let s = db.analyze("SELECT 1::int2::int8 AS n").unwrap();
    assert_cols(&s, vec![c("n", int8())]);
}

#[test]
fn numeric_tower_int4_to_numeric() {
    let db = setup();
    let s = db.analyze("SELECT 1::int4::numeric AS n").unwrap();
    assert_cols(&s, vec![c("n", numeric())]);
}

#[test]
fn numeric_tower_numeric_to_float8() {
    let db = setup();
    let s = db.analyze("SELECT (1::numeric)::float8 AS n").unwrap();
    assert_cols(&s, vec![c("n", float8())]);
}

#[test]
fn numeric_tower_float4_to_float8() {
    let db = setup();
    let s = db.analyze("SELECT (1.0::float4)::float8 AS n").unwrap();
    assert_cols(&s, vec![c("n", float8())]);
}

// ── Array cast ───────────────────────────────────────────────────────────────

#[test]
fn cast_array_literal_to_int8_array() {
    let db = setup();
    let s = db.analyze("SELECT ARRAY[1,2]::int8[] AS xs").unwrap();
    assert_cols(&s, vec![c("xs", array_of(int8()))]);
}

// ── Cast inside a VALUES list projected from a subquery ──────────────────────

#[test]
fn cast_in_values_subquery() {
    let db = setup();
    // VALUES infers the column type from the first row's expressions; the
    // cast pins the element type so downstream consumers see int8.
    let s = db
        .analyze("SELECT x FROM (VALUES (1::int8), (2)) AS t(x)")
        .unwrap();
    assert_cols(&s, vec![c("x", int8())]);
}

// ── Domain → base ────────────────────────────────────────────────────────────

fn setup_with_domain() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0);
         CREATE TABLE accounts (
            id      BIGINT PRIMARY KEY,
            balance positive_int NOT NULL
         );",
    )
    .unwrap();
    db
}

#[test]
fn domain_cast_to_base_strips_domain_wrapper() {
    let db = setup_with_domain();
    // `balance::int4` peels the domain off and lands on the base type.
    let s = db
        .analyze("SELECT balance::int4 AS b FROM accounts")
        .unwrap();
    assert_cols(&s, vec![c("b", int4())]);
}

#[test]
fn domain_select_preserves_domain_wrapper() {
    let db = setup_with_domain();
    // Without an explicit cast the domain wrapper survives in the output —
    // matches PG's RowDescription, which reports the domain OID.
    let s = db.analyze("SELECT balance FROM accounts").unwrap();
    assert_cols(
        &s,
        vec![c("balance", domain("public", "positive_int", int4()))],
    );
}

#[test]
fn coalesce_domain_and_base_resolves_to_base() {
    let db = setup_with_domain();
    // `COALESCE(domain_over_int4, bigint)` — PG smashes the domain to its base
    // (int4), so the common type is int8. The analyzer used to over-reject it.
    let s = db
        .analyze("SELECT COALESCE(balance, id) AS c FROM accounts")
        .unwrap();
    assert_cols(&s, vec![c("c", int8())]);
}

#[test]
fn coalesce_domain_mismatch_reports_base_type_name() {
    let db = setup_with_domain();
    // The "cannot be matched" wording reports the domain's *base* (`integer`),
    // matching PG, not the domain name `positive_int`.
    assert_analyze_err!(
        db.analyze("SELECT COALESCE(balance, true) FROM accounts"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "COALESCE types integer and boolean cannot be matched\n",
            "  help: add an explicit cast so the branches share a type, e.g. `expr::boolean`\n",
        ),
    );
}

#[test]
fn domain_implicit_arithmetic_falls_back_to_base() {
    let db = setup_with_domain();
    // `balance + 1` — the `+` operator is defined on the base int4, so the
    // result is int4, not the domain. Same as PG's behavior.
    let s = db
        .analyze("SELECT balance + 1 AS bigger FROM accounts")
        .unwrap();
    assert_cols(&s, vec![c("bigger", int4())]);
}

// ── citext (extension) ↔ text ───────────────────────────────────────────────

fn setup_citext() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE EXTENSION citext;
         CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            email citext NOT NULL
         );",
    )
    .unwrap();
    db
}

#[test]
fn citext_column_keeps_citext_type() {
    let db = setup_citext();
    let s = db.analyze("SELECT email FROM users").unwrap();
    // citext lives in `public` once the extension is created.
    assert_cols(
        &s,
        vec![c("email", basic_ext("public", "citext", "citext"))],
    );
}

#[test]
fn cast_text_to_citext() {
    let db = setup_citext();
    // `'x'::citext` — bare `citext` (unqualified) resolves through search_path.
    let s = db.analyze("SELECT 'x'::citext AS e").unwrap();
    assert_cols(&s, vec![c("e", basic_ext("public", "citext", "citext"))]);
}

#[test]
fn cast_citext_to_text() {
    let db = setup_citext();
    let s = db.analyze("SELECT email::text AS e FROM users").unwrap();
    assert_cols(&s, vec![c("e", text())]);
}

// ── Mixed-type array literal: common-type resolution ────────────────────────

#[test]
fn array_literal_int_and_numeric_resolves_to_numeric() {
    let db = setup();
    // PG: ARRAY[1, 2.5] resolves the common type to numeric (int4 → numeric).
    let s = db.analyze("SELECT ARRAY[1, 2.5] AS xs").unwrap();
    assert_cols(&s, vec![c("xs", array_of(numeric()))]);
}

#[test]
fn array_literal_int4_and_int8_resolves_to_int8() {
    let db = setup();
    // ARRAY[1, 2::int8] — int4 + int8 → int8.
    let s = db.analyze("SELECT ARRAY[1, 2::int8] AS xs").unwrap();
    assert_cols(&s, vec![c("xs", array_of(int8()))]);
}

#[test]
fn array_literal_int_and_float8_resolves_to_float8() {
    let db = setup();
    let s = db.analyze("SELECT ARRAY[1, 2::float8] AS xs").unwrap();
    assert_cols(&s, vec![c("xs", array_of(float8()))]);
}

#[test]
fn array_literal_incompatible_types_rejected() {
    let db = setup();
    // PG: `ARRAY types text and integer cannot be matched` — once a
    // concrete type pins the element category, the analyzer must reject
    // siblings that don't fit. Note: bare `'x'` (unknown) + `1` is fine
    // both in PG and here (PG defers to runtime); the test below uses
    // explicit `'x'::text` to force a real type clash at parse time.
    assert_analyze_err!(
        db.analyze("SELECT ARRAY['x'::text, 1]"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "ARRAY types text and integer cannot be matched\n",
            "  help: cast the elements to a common type, e.g. `elem::text`\n",
        ),
    );
}

#[test]
fn array_literal_bool_and_int_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT ARRAY[true, 1]"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "ARRAY types boolean and integer cannot be matched\n",
            "  help: cast the elements to a common type, e.g. `elem::boolean`\n",
        ),
    );
}

#[test]
fn array_literal_with_invalid_unknown_content_rejected() {
    // `'x'` is an unknown literal coerced to the int4 branch's type; PG runs
    // `int4in('x')` at parse_analyze time and raises `invalid input syntax
    // for type integer: "x"`. The analyzer mirrors that via `literal_input`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT ARRAY['x', 1] AS xs"),
        AnalyzeError::InvalidLiteral(_),
        concat!(
            "invalid input syntax for type integer: \"x\"\n",
            "  ╭────\n",
            "1 │ SELECT ARRAY['x', 1] AS xs\n",
            "  ·              ─┬─\n",
            "  ·               ╰─ this literal\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn array_literal_with_valid_unknown_content_coerced() {
    // The flip side: `'2'` parses fine as int4, so the array lands on
    // `int4[]` exactly like PG.
    let db = setup();
    let s = db.analyze("SELECT ARRAY['2', 1] AS xs").unwrap();
    assert_cols(&s, vec![c("xs", array_of(int4()))]);
}

// ── Domain preservation through ::base round-trip ───────────────────────────

#[test]
fn domain_cast_to_base_then_back_returns_base() {
    let db = setup_with_domain();
    // `(balance::int4)` strips the domain. There's no implicit way back —
    // the analyzer should not silently reattach the domain wrapper.
    let s = db
        .analyze("SELECT (balance::int4) AS b FROM accounts")
        .unwrap();
    assert_cols(&s, vec![c("b", int4())]);
}

// ── Type modifier (atttypmod) is modeled ────────────────────────────────────
//
// PG carries precision/scale (numeric(p,s), varchar(n), etc.) in
// `pg_attribute.atttypmod`. The catalog mirror tracks it, so the analyzer
// rejects literal assignments that overflow the declared precision before
// they reach PG.

#[test]
fn numeric_typmod_overflow_should_be_rejected() {
    // The analyzer catches `12345.67` overflowing `numeric(4,2)` at compile
    // time; real PG only complains on execution, and pglite's `prepare`
    // skips that pass — opt out of the mirror.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, amount NUMERIC(4,2) NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("INSERT INTO t (id, amount) VALUES ($p1, 12345.67)"),
        AnalyzeError::Invalid(_),
        "numeric field overflow: a field with precision 4, scale 2 must round to an absolute value less than 10^2",
    );
}

#[test]
fn varchar_typmod_string_literal_too_long_should_be_rejected() {
    // Same shape as the numeric overflow test above — analyzer flags too-
    // long literals at compile time, real PG only at execution. Opt out of
    // the pglite mirror.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (slug VARCHAR(3) NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("INSERT INTO t (slug) VALUES ('toolong')"),
        AnalyzeError::Invalid(_),
        "value too long for type character varying(3)",
    );
}

// ── UndefinedType: typo in `::type` produces snippet + hint ────────────────

#[test]
fn cast_to_unknown_type_renders_snippet_and_hint() {
    // `int32` doesn't exist in PG (it's `int4`). Caret points at the type
    // name and the hint suggests the nearest real type.
    let db = PgCatalog::new().unwrap();
    assert_analyze_err!(
        db.analyze("SELECT 1::int32"),
        AnalyzeError::UndefinedType(_),
        "\
type \"int32\" does not exist
  ╭────
1 │ SELECT 1::int32
  ·           ──┬──
  ·             ╰─ type does not exist
  ╰────
  help: did you mean \"int2\"?
",
    );
}

#[test]
fn cast_to_unrelated_type_has_no_hint() {
    // `xyzabc` is too far from any catalog type — no hint.
    let db = PgCatalog::new().unwrap();
    assert_analyze_err!(
        db.analyze("SELECT 1::xyzabc"),
        AnalyzeError::UndefinedType(_),
        "\
type \"xyzabc\" does not exist
  ╭────
1 │ SELECT 1::xyzabc
  ·           ───┬──
  ·              ╰─ type does not exist
  ╰────
",
    );
}

// ── Illegal explicit casts (no pg_cast path, neither side a string) ─────────

#[test]
fn cast_bool_to_float8_rejected() {
    let db = PgCatalog::new().unwrap();
    // There is no cast path boolean → double precision, and neither side is a
    // string type, so PG rejects it at parse time. The analyzer used to accept
    // any explicit cast.
    assert_analyze_err!(
        db.analyze("SELECT true::float8"),
        AnalyzeError::Invalid(_),
        concat!(
            "cannot cast type boolean to double precision\n",
            "  ╭────\n",
            "1 │ SELECT true::float8\n",
            "  ·        ──┬─\n",
            "  ·          ╰─ this is boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn cast_timestamptz_to_bytea_rejected() {
    let db = PgCatalog::new().unwrap();
    assert_analyze_err!(
        db.analyze("SELECT now()::bytea"),
        AnalyzeError::Invalid(_),
        concat!(
            "cannot cast type timestamp with time zone to bytea\n",
            "  ╭────\n",
            "1 │ SELECT now()::bytea\n",
            "  ·        ─┬─\n",
            "  ·         ╰─ this is timestamp with time zone\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn cast_date_to_bool_rejected() {
    let db = PgCatalog::new().unwrap();
    assert_analyze_err!(
        db.analyze("SELECT '2020-01-01'::date::bool"),
        AnalyzeError::Invalid(_),
        "cannot cast type date to boolean",
    );
}

#[test]
fn cast_bool_to_int4_still_allowed() {
    // Guard against over-rejection: bool → int4 has a real pg_cast entry, so
    // it must keep working.
    let db = PgCatalog::new().unwrap();
    let s = db.analyze("SELECT true::int4 AS n").unwrap();
    assert_cols(&s, vec![c("n", int4())]);
}

#[test]
fn internal_char_type_rendered_quoted_in_messages() {
    // The internal single-byte type (OID 18) must render as `"char"` (quoted)
    // in error messages, exactly like PG — bare `char` would read as SQL
    // `char`/`bpchar`. PG: `cannot cast type numeric to "char"`.
    let db = PgCatalog::new().unwrap();
    assert_analyze_err!(
        db.analyze("SELECT (3.14)::\"char\""),
        AnalyzeError::Invalid(_),
        concat!(
            "cannot cast type numeric to \"char\"\n",
            "  ╭────\n",
            "1 │ SELECT (3.14)::\"char\"\n",
            "  ·         ──┬─\n",
            "  ·           ╰─ this is numeric\n",
            "  ╰────\n",
        ),
    );
}

// ── Function-call-as-cast (FUNCDETAIL_COERCION) ─────────────────────────────

#[test]
fn type_name_call_acts_as_cast() {
    // `float8(x)` / `text(x)` with no matching function are casts in PG.
    let db = setup();
    let s = db.analyze("SELECT float8(age) AS v FROM users").unwrap();
    assert_cols(&s, vec![cn("v", float8())]);
    let s = db.analyze("SELECT text(id) AS v FROM users").unwrap();
    assert_cols(&s, vec![c("v", text())]);
    let s = db
        .analyze("SELECT pg_catalog.float8(name::int4) AS v FROM users")
        .unwrap();
    assert_cols(&s, vec![c("v", float8())]);
    // Literal contents validate exactly like `'x'::float8` would.
    let err = db.analyze("SELECT float8('x')").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("invalid input syntax for type double precision: \"x\""),
        "got: {err}"
    );
}

#[test]
fn typename_followed_by_a_parameter_is_a_syntax_error() {
    // `int4 $1` parses only in libpg_query (a normalization extension);
    // PG's grammar takes a string constant there (PG 18: syntax error).
    let db = setup();
    for sql in ["SELECT int4 $r", "SELECT interval $r AS a"] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::SyntaxError(_),
            "syntax error at or near \"$1\""
        );
    }
    // The ordinary casts still work.
    for sql in [
        "SELECT $r::int4 AS a",
        "SELECT CAST($r AS int4) AS a",
        "SELECT int4 '1' AS a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

// ── Nullability of casts and operators whose function can yield NULL ──────

#[test]
fn jsonb_to_scalar_casts_are_nullable() {
    // int4(jsonb) & co. return NULL for a JSON null (PG 18:
    // 'null'::jsonb::int4 IS NULL); jsonb → text is I/O and never NULL.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE tj (j jsonb NOT NULL);").unwrap();
    let s = db
        .analyze(
            "SELECT 'null'::jsonb::int4 AS a, j::int4 AS b, j::bool AS c, j::numeric AS d, \
             j::float8 AS e, j::int8 AS f, j::int2 AS g, j::float4 AS h, j::text AS i, \
             j::json AS k FROM tj",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("a", int4()),
            cn("b", int4()),
            cn("c", bool_ty()),
            cn("d", numeric()),
            cn("e", float8()),
            cn("f", int8()),
            cn("g", int2()),
            cn("h", float4()),
            c("i", text()),
            c("k", json_ty()),
        ],
    );
}

#[test]
fn jsonpath_operators_are_nullable() {
    // jsonb_path_exists_opr / jsonb_path_match_opr return NULL when the
    // path evaluation fails ('{}'::jsonb @? 'strict $.a' IS NULL on PG 18).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE tj (j jsonb NOT NULL, t tsvector NOT NULL);")
        .unwrap();
    let s = db
        .analyze(
            "SELECT j @? 'strict $.a' AS a, j @@ '$.a == 1' AS b, \
             t @@ 'x'::tsquery AS c FROM tj",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("a", bool_ty()), cn("b", bool_ty()), c("c", bool_ty())],
    );
}
