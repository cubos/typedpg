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

#[test]
fn user_defined_range_casts_to_its_multirange() {
    // CREATE TYPE ... AS RANGE also creates the multirange and registers a
    // range → multirange cast (`pg_cast`, castmethod 'f', explicit), backed
    // by the multirange constructor taking one range.
    let mut db = setup();
    db.apply_sql(r#"CREATE TYPE textrange AS RANGE (subtype = text, collation = "C");"#)
        .unwrap();
    let s = db
        .analyze("SELECT textrange('a', 'c')::textmultirange AS m")
        .unwrap();
    assert_eq!(
        s.columns[0].pg_type.cast_name().unwrap(),
        "public.textmultirange"
    );
    db.analyze("SELECT 'empty'::textrange::textmultirange")
        .unwrap();
    // The cast function is strict: a NULL range stays NULL.
    let s = db
        .analyze("SELECT NULL::textrange::textmultirange AS m")
        .unwrap();
    assert!(s.columns[0].nullable);
    // Explicit only: no assignment cast.
    db.apply_sql("CREATE TABLE spans (m textmultirange);")
        .unwrap();
    let err = db
        .analyze("INSERT INTO spans VALUES (textrange('a', 'c'))")
        .unwrap_err();
    assert!(
        err.to_string().starts_with(
            "column \"m\" is of type textmultirange but expression is of type textrange"
        ),
        "{err}"
    );
    // The cast goes with the types: re-creating them doesn't collide.
    db.apply_sql(
        r#"DROP TABLE spans;
           DROP TYPE textrange;
           CREATE TYPE textrange AS RANGE (subtype = text, collation = "C");"#,
    )
    .unwrap();
    db.analyze("SELECT textrange('a', 'c')::textmultirange")
        .unwrap();
}

#[test]
fn casts_to_polymorphic_pseudo_types_keep_the_operand_type() {
    // coerce_type returns the operand of a cast to "any", anyelement,
    // anynonarray, anycompatible or anycompatiblenonarray as it is; the
    // array / range / multirange ones need an actual such value, relabeled
    // from a domain to its base type. An untyped literal or parameter stays
    // unknown (text) under the former, and fails the pseudo-type's input
    // function (a literal) or takes the pseudo-type (a parameter) under
    // the latter.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE DOMAIN ia AS int[];").unwrap();
    let s = db
        .analyze(
            "SELECT 1::anyelement AS a, 1::\"any\" AS b, 1::anycompatible AS c, \
                    'x'::text::anynonarray AS d, '1'::anyelement AS e, \
                    NULL::anyelement AS f, ARRAY[1]::anyarray AS g, \
                    '{1}'::ia::anyarray AS h, ARRAY[1]::anycompatiblearray AS i, \
                    int4range(1, 2)::anyrange AS j",
        )
        .unwrap();
    let got: Vec<_> = s
        .columns
        .iter()
        .map(|c| c.pg_type.cast_name().unwrap())
        .collect();
    assert_eq!(
        got,
        [
            "pg_catalog.int4",
            "pg_catalog.int4",
            "pg_catalog.int4",
            "pg_catalog.text",
            "pg_catalog.text",
            "pg_catalog.text",
            "pg_catalog.int4[]",
            "pg_catalog.int4[]",
            "pg_catalog.int4[]",
            "pg_catalog.int4range",
        ]
    );
    let s = db.analyze("SELECT $1::anyelement AS v").unwrap();
    assert_eq!(s.params[0].pg_type, text());
    assert_eq!(s.columns[0].pg_type, text());
    let s = db.analyze("SELECT $1::int::anyelement AS v").unwrap();
    assert_eq!(s.columns[0].pg_type, int4());
    for (sql, msg) in [
        (
            "SELECT '{1}'::anyarray",
            "cannot accept a value of type anyarray",
        ),
        (
            "SELECT 'x'::anyrange",
            "cannot accept a value of type anyrange",
        ),
        (
            "SELECT 'x'::anycompatiblemultirange",
            "cannot accept a value of type anycompatiblemultirange",
        ),
        (
            "SELECT 'x'::trigger",
            "cannot accept a value of type trigger",
        ),
        ("SELECT 'x'::anyenum", "cannot cast type unknown to anyenum"),
        ("SELECT 1::anyarray", "cannot cast type integer to anyarray"),
        // Nothing converts to or from `internal`, an untyped value included.
        ("SELECT 1::internal", "cannot cast type integer to internal"),
        (
            "SELECT 'x'::internal",
            "cannot cast type unknown to internal",
        ),
        (
            "SELECT $1::internal",
            "cannot cast type unknown to internal",
        ),
        (
            "SELECT NULL::internal",
            "cannot cast type unknown to internal",
        ),
    ] {
        let err = db
            .analyze(sql)
            .err()
            .unwrap_or_else(|| panic!("PG rejects `{sql}`"))
            .to_string();
        assert!(err.starts_with(msg), "`{sql}`: {err}");
    }
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
            "  ╭────\n",
            "1 │ SELECT COALESCE(balance, true) FROM accounts\n",
            "  ·                          ──┬─\n",
            "  ·                            ╰─ this is boolean\n",
            "  ╰────\n",
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
            "  ╭────\n",
            "1 │ SELECT ARRAY['x'::text, 1]\n",
            "  ·                         ┬\n",
            "  ·                         ╰─ this is integer\n",
            "  ╰────\n",
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
            "  ╭────\n",
            "1 │ SELECT ARRAY[true, 1]\n",
            "  ·                    ┬\n",
            "  ·                    ╰─ this is integer\n",
            "  ╰────\n",
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
    assert_err_prefix!(
        db.analyze("INSERT INTO t (id, amount) VALUES ($p1, 12345.67)"),
        AnalyzeError::NumericValueOutOfRange(_),
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

// ── Constants the planner folds ─────────────────────────────────────────────
//
// A conversion or an arithmetic operator over constants is a function call
// after parse analysis (`int2(40000)`, `int4pl(2147483647, 1)`), which the
// planner's `eval_const_expressions` runs while it plans the statement: one
// that fails fails every execution, before any row is read — PREPARE
// succeeds, EXECUTE never does. The analyzer reports them at compile time
// (the oracle's execute fallback checks the error), but only where the
// planner certainly folds: not past a CASE condition / COALESCE argument /
// AND-OR argument that may fold first, nor in a subquery whose outputs may
// go unread.

fn folding_setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN small AS int2;
         CREATE TABLE s (k int PRIMARY KEY, a int2, b int4, c int8, d small,
             n numeric(3,1), v varchar(3));
         CREATE TABLE ident (id int GENERATED BY DEFAULT AS IDENTITY, x int);
         CREATE TABLE ruled (a int2, v varchar(3));
         CREATE RULE ruled_nothing AS ON INSERT TO ruled DO INSTEAD NOTHING;",
    )
    .unwrap();
    db
}

#[test]
fn integer_constants_assigned_out_of_range_are_rejected() {
    let db = folding_setup();
    for (sql, msg) in [
        (
            "INSERT INTO s (k, a) VALUES (1, 40000)",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) VALUES (1, -32769)",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, b) VALUES (1, 3000000000)",
            "integer out of range",
        ),
        (
            "INSERT INTO s (k, b) VALUES (1, 2147483647 + 1)",
            "integer out of range",
        ),
        (
            "INSERT INTO s (k, c) VALUES (1, 9223372036854775808)",
            "bigint out of range",
        ),
        (
            "INSERT INTO s (k, d) VALUES (1, 40000)",
            "smallint out of range",
        ),
        // A `numeric` is rounded first: 32767.5 is 32768.
        (
            "INSERT INTO s (k, a) VALUES (1, 32767.5)",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) VALUES (1, 1.5e10)",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) VALUES (1, 1), (2, 40000)",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) VALUES (1, '1'::int4 * 40000)",
            "smallint out of range",
        ),
        // Planned whatever the rows: no row matches, no row is read.
        (
            "UPDATE s SET a = 40000 WHERE false",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET b = 3000000000",
            "integer out of range",
        ),
        (
            "MERGE INTO s USING (SELECT 1 AS k) x ON s.k = x.k \
             WHEN MATCHED THEN UPDATE SET a = 40000",
            "smallint out of range",
        ),
        (
            "MERGE INTO s USING (SELECT 1 AS k) x ON s.k = x.k \
             WHEN NOT MATCHED THEN INSERT (k, a) VALUES (1, 40000)",
            "smallint out of range",
        ),
        // A plain SELECT is pulled up into the INSERT, its outputs folded
        // into the INSERT's target list.
        (
            "INSERT INTO s (k, a) SELECT k, 40000 FROM s",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) SELECT k, 40000 FROM s WHERE b > 0",
            "smallint out of range",
        ),
        // A function that is neither aggregate, set-returning nor volatile
        // doesn't keep it from being pulled up; a subquery it reads is
        // pulled up in turn, its output substituted.
        (
            "INSERT INTO s (k, a) SELECT abs(k), 40000 FROM s",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) SELECT 1, x FROM (SELECT 40000 AS x) q",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) SELECT 1, x FROM (SELECT x FROM (SELECT 40000 AS x) q1) q2",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) SELECT 1, x FROM (VALUES (40000)) v(x)",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) SELECT s.k, q.x FROM s JOIN (SELECT 40000 AS x) q ON true",
            "smallint out of range",
        ),
        (
            "INSERT INTO s (k, a) SELECT 1, x FROM (SELECT 40000 AS x) q WHERE false",
            "smallint out of range",
        ),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::NumericValueOutOfRange(_),
            msg
        );
    }
}

#[test]
fn integer_constants_assigned_in_range_or_never_converted_are_accepted() {
    let db = folding_setup();
    for sql in [
        "INSERT INTO s (k, a) VALUES (1, 32767)",
        "INSERT INTO s (k, a) VALUES (1, -32768)",
        "INSERT INTO s (k, a) VALUES (1, 32767.4)",
        "INSERT INTO s (k, b) VALUES (1, 2147483646 + 1)",
        "INSERT INTO s (k, c) VALUES (1, 9223372036854775807)",
        // Not pulled up (LIMIT, a volatile function): converted row by
        // row, and there may be none.
        "INSERT INTO s (k, a) SELECT k, 40000 FROM s LIMIT 1",
        "INSERT INTO s (k, a, b) SELECT k, 40000, random()::int FROM s",
        "INSERT INTO s (k, a, b) SELECT 1, 40000, generate_series(1, 0)",
        // On the nullable side of an outer join a PlaceHolderVar keeps the
        // pulled-up constant from being folded; an unread one never is.
        "INSERT INTO s (k, a) SELECT s.k, q.x FROM s LEFT JOIN (SELECT 40000 AS x) q ON true",
        "INSERT INTO s (k, a) SELECT 1, 4 FROM (SELECT 40000 AS x) q",
        "INSERT INTO s (k, a) SELECT 1, x FROM (SELECT 4 AS x) q",
        // OVERRIDING USER VALUE discards the value unconverted.
        "INSERT INTO ident (id) OVERRIDING USER VALUE VALUES (3000000000)",
        // An INSTEAD rule replaces the statement: never planned.
        "INSERT INTO ruled (a) VALUES (40000)",
        "INSERT INTO ruled (v) VALUES ('abcd')",
        // An explicit cast truncates first (`::char` is `char(1)`).
        "INSERT INTO s (k, v) VALUES (1, 'abcd'::varchar(2))",
        "INSERT INTO s (k, v) VALUES (1, 'abcd'::char)",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
}

#[test]
fn numeric_constants_folded_into_a_typmod_overflow_are_rejected() {
    let db = folding_setup();
    for sql in [
        "INSERT INTO s (k, n) VALUES (1, 99 + 1)",
        "INSERT INTO s (k, n) VALUES (1, 100::int2)",
        "SELECT (99 + 1)::numeric(3,1)",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::NumericValueOutOfRange(_),
            "numeric field overflow: a field with precision 3, scale 1 must round to an absolute value less than 10^2"
        );
    }
    // A string cast to text keeps its length; to varchar(n), it doesn't.
    assert_err_prefix!(
        db.analyze("INSERT INTO s (k, v) VALUES (1, 'abcd'::text)"),
        AnalyzeError::Invalid(_),
        "value too long for type character varying(3)"
    );
    db.analyze("INSERT INTO s (k, n) VALUES (1, 98 + 1)")
        .unwrap();
}

#[test]
fn constant_casts_and_arithmetic_that_fail_are_rejected() {
    let db = folding_setup();
    for (sql, msg) in [
        ("SELECT 40000::int2", "smallint out of range"),
        ("SELECT 40000::small", "smallint out of range"),
        ("SELECT -32768::int2", "smallint out of range"),
        ("SELECT '40000'::int4::int2", "smallint out of range"),
        ("SELECT 32767::int2 + 1::int2", "smallint out of range"),
        ("SELECT 2147483647 + 1", "integer out of range"),
        ("SELECT 2147483647 + '1'", "integer out of range"),
        ("SELECT -2147483647 - 2", "integer out of range"),
        ("SELECT 65536 * 65536", "integer out of range"),
        ("SELECT (-2147483648) / -1", "integer out of range"),
        ("SELECT 9223372036854775807 + 1", "bigint out of range"),
        ("SELECT 1e400::int8", "bigint out of range"),
        ("SELECT 32767.5::int2", "smallint out of range"),
        ("SELECT 1 LIMIT 1e30", "bigint out of range"),
        ("SELECT (ARRAY[1, 2])[3000000000]", "integer out of range"),
        // In a WHERE no row passes, a CASE branch after a condition that
        // reads a column: still folded.
        (
            "SELECT 2147483647 + 1 FROM s WHERE false",
            "integer out of range",
        ),
        (
            "SELECT CASE WHEN b > 0 THEN 40000::int2 END FROM s",
            "smallint out of range",
        ),
        (
            "SELECT COALESCE(a, 40000::int2) FROM s",
            "smallint out of range",
        ),
        (
            "SELECT k FROM s WHERE a = 1 OR 40000::int2 = a",
            "smallint out of range",
        ),
        ("SELECT (SELECT 40000::int2)", "smallint out of range"),
        (
            "SELECT 1 UNION ALL SELECT 40000::int2",
            "smallint out of range",
        ),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::NumericValueOutOfRange(_),
            msg
        );
    }
    for sql in [
        "SELECT 1 / 0",
        "SELECT 1 % 0",
        "SELECT 1 / 0.0",
        "SELECT 1.5 % 0",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DivisionByZero(_),
            "division by zero"
        );
    }
    for (sql, msg) in [
        (
            "SELECT 'NaN'::numeric::int4",
            "cannot convert NaN to integer",
        ),
        (
            "SELECT 'Infinity'::numeric::int2",
            "cannot convert infinity to smallint",
        ),
        (
            "SELECT '-inf'::numeric::int8",
            "cannot convert infinity to bigint",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::FeatureNotSupported(_), msg);
    }
}

#[test]
fn constants_the_planner_never_folds_are_accepted() {
    let db = folding_setup();
    for sql in [
        "SELECT 32767::int2, (-32768)::int4::int2, 2147483646 + 1",
        // `-(-2147483648)`: the grammar folds both signs into the constant,
        // a bigint.
        "SELECT -(-2147483648)",
        // `int4mod`'s special case.
        "SELECT (-2147483648) % -1",
        "SELECT 'NaN'::numeric / 0",
        // A WHEN folded to FALSE is dropped, and everything after one
        // folded to TRUE; COALESCE stops at its first non-NULL constant,
        // AND / OR at their first FALSE / TRUE.
        "SELECT CASE WHEN false THEN 40000::int2 END",
        "SELECT CASE WHEN 1 = 1 THEN 1 ELSE 1 / 0 END",
        "SELECT CASE 1 WHEN 1 THEN 1 WHEN 2 THEN 40000::int2 END",
        "SELECT COALESCE(1, 40000::int2)",
        "SELECT 1 WHERE 1 = 1 OR 40000::int2 = 1",
        "SELECT 1 WHERE false AND 1 / 0 = 1",
        // An output nothing reads is dropped before it is folded; EXISTS
        // discards its target list; a SELECT CTE nothing reads is never
        // planned.
        "SELECT a FROM (SELECT 1 AS a, 40000::int2 AS b) x",
        "SELECT 1 FROM s WHERE EXISTS (SELECT 40000::int2)",
        "WITH x AS (SELECT 40000::int2) SELECT 1",
        // A column, not a constant: fails only for the rows it reads.
        "SELECT b::int2 FROM s",
        "SELECT b + 2147483647 FROM s",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
}

#[test]
fn conditions_that_may_fold_leave_what_follows_unfolded() {
    // Folding reaches past constants: `x AND false` is FALSE, `x OR true`
    // TRUE, a strict operator over NULL is NULL, a row test or comparison
    // goes field by field, a pulled-up subquery's or view's column is its
    // expression, a SQL function is inlined. A CASE branch, COALESCE
    // argument or AND / OR argument after such a condition may be dropped
    // unfolded.
    let mut db = folding_setup();
    db.apply_sql(
        "CREATE VIEW zero AS SELECT 0 AS c;
         CREATE FUNCTION yes(int) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true';
         CREATE TABLE gen (k int, g int GENERATED ALWAYS AS (5) VIRTUAL);",
    )
    .unwrap();
    for sql in [
        "SELECT CASE WHEN b > 0 AND false THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE WHEN b > 0 OR true THEN 1 ELSE 1 / 0 END FROM s",
        "SELECT CASE WHEN NULL + b > 0 THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE WHEN ROW(b, 1) IS NULL THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE WHEN ROW(b, 1) = ROW(k, 3) THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE WHEN b IN (NULL) THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE b WHEN NULL THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE WHEN $1::int > b THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT COALESCE(b > 0 OR true, 1 / 0 = 1) FROM s",
        "SELECT k FROM s WHERE (b > 0 OR true) OR 1 / 0 = 1",
        "SELECT k FROM s WHERE (b > 0 AND false) AND 1 / 0 = 1",
        "SELECT CASE WHEN x.c > 0 THEN 1 / 0 ELSE 2 END FROM (SELECT 0 AS c) x",
        "SELECT CASE WHEN x.c > 0 THEN 1 / 0 ELSE 2 END FROM s, (SELECT 0 AS c) x",
        "SELECT CASE WHEN c > 0 THEN 1 / 0 ELSE 2 END FROM zero",
        "SELECT CASE WHEN yes(b) THEN 1 ELSE 1 / 0 END FROM s",
        "SELECT CASE WHEN g < 0 THEN 1 / 0 ELSE 2 END FROM gen",
        // A sublink is planned after the expression around it is folded:
        // one that folds away, whichever side the constant is on, drops
        // it. EXISTS drops its target list and ORDER BY first.
        "SELECT 1 WHERE (SELECT 1 / 0) = 1 AND false",
        "SELECT 1 WHERE EXISTS (SELECT 1 WHERE 1 / 0 = 1) AND false",
        "SELECT (SELECT 40000::int2) IS NULL OR true",
        "SELECT (SELECT 1 / 0) + NULL",
        "SELECT 1 FROM s WHERE EXISTS (SELECT 1 FROM s s2 ORDER BY 1 / 0)",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
    // What a table's column makes certain is still folded: a NOT NULL
    // column's `IS NULL` isn't reduced while constants are folded, a
    // sublink never is, a NULL argument doesn't stop COALESCE or OR.
    for sql in [
        "SELECT CASE WHEN k IS NULL THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE b WHEN 1 THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE WHEN abs(b) > 0 THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE WHEN b IS DISTINCT FROM 1 THEN 1 / 0 ELSE 2 END FROM s",
        "SELECT CASE WHEN EXISTS (SELECT 1) THEN 1 ELSE 1 / 0 END",
        "SELECT COALESCE(NULL, b, 1 / 0) FROM s",
        "SELECT COALESCE(NULL::int, 1 / 0) FROM s",
        "SELECT COALESCE(NULL + b, 1 / 0) FROM s",
        "SELECT k FROM s WHERE NULL + b > 0 OR 1 / 0 = 1",
        "SELECT CASE WHEN x.c > 0 THEN 1 / 0 ELSE 2 END FROM (SELECT b AS c FROM s) x",
        "SELECT CASE WHEN x.c > 0 THEN 1 / 0 END FROM (SELECT b AS c FROM s LIMIT 5) x",
        "SELECT CASE WHEN t2.b > 0 THEN 1 / 0 END FROM s LEFT JOIN s t2 ON true",
        "SELECT 1 FROM s WHERE (SELECT 1 / 0) = 1 AND b > 0",
        "SELECT 1 WHERE EXISTS (SELECT 1 WHERE 1 / 0 = 1)",
        "SELECT 1 FROM s WHERE EXISTS (SELECT 1 FROM s s2 WHERE 1 / 0 = 1) AND b > 0",
        "SELECT 1 FROM s WHERE EXISTS (SELECT 1 FROM (SELECT 1 / 0 AS q) x WHERE x.q = s.b)",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DivisionByZero(_),
            "division by zero"
        );
    }
}

#[test]
fn a_subquery_output_fails_where_it_is_read() {
    // A pulled-up subquery's (VALUES list's, inlined CTE's) output is
    // substituted where the outer query reads it, and folded there; its
    // WHERE joins the outer query's. A VALUES list of several rows and a
    // MATERIALIZED CTE are planned whole.
    let db = folding_setup();
    for sql in [
        "SELECT x.q FROM (SELECT 1 / 0 AS q) x",
        "SELECT * FROM (VALUES (1 / 0)) v",
        "SELECT v.column1 FROM s, (VALUES (1 / 0)) v",
        "SELECT k FROM s, (VALUES (1), (1 / 0)) v",
        "SELECT k FROM s, (SELECT * FROM (VALUES (1), (1 / 0)) v) x WHERE false",
        "SELECT l.z FROM s, LATERAL (SELECT 1 / 0 AS z) l",
        "WITH c AS (SELECT 1 / 0 AS q) SELECT q FROM c",
        "WITH c AS MATERIALIZED (SELECT 1 / 0 AS q) SELECT q FROM c",
        "WITH c AS MATERIALIZED (SELECT 1 / 0 AS q, 1 AS r) SELECT r FROM c",
        "WITH c AS (SELECT 1 AS q FROM s WHERE 1 / 0 = 1) SELECT 1 FROM c",
        "WITH c AS (SELECT 1 / 0 AS q) SELECT (SELECT q FROM c)",
        "SELECT 1 FROM (SELECT 1 FROM s WHERE 1 / 0 = 1) x",
        "SELECT x.q FROM s LEFT JOIN (SELECT 1 / 0 AS q) x ON true",
        "SELECT x.q FROM (SELECT 1 / 0 AS q) x WHERE false",
        "SELECT x FROM (SELECT 1 / 0 AS q) x",
        "SELECT x.* FROM (SELECT 1 / 0 AS q) x",
        "SELECT y.q FROM (SELECT x.q FROM (SELECT 1 / 0 AS q) x) y",
        "SELECT 1 FROM (SELECT 1 / 0 AS q) x ORDER BY x.q",
        "SELECT 1 FROM s JOIN (SELECT 1 / 0 AS q) x ON x.q = s.b",
        // A WINDOW clause no call uses is planned (its keys as resjunk
        // entries) at a level of its own.
        "SELECT k FROM s WINDOW w AS (PARTITION BY 1 / 0)",
        "SELECT x.k FROM (SELECT k FROM s WINDOW w AS (ORDER BY 1 / 0) OFFSET 0) x",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DivisionByZero(_),
            "division by zero"
        );
    }
    for sql in [
        "SELECT * FROM (VALUES (2147483647 + 1)) q",
        "SELECT l.z FROM s, LATERAL (SELECT 2147483647 + 1 AS z) l",
        "SELECT x.k FROM (SELECT k FROM s WINDOW w AS (ROWS 2147483647 + 1 PRECEDING) OFFSET 0) x",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::NumericValueOutOfRange(_),
            "integer out of range"
        );
    }
    // Unread, or read only where the planner may not reach — or in a
    // subquery it doesn't pull up, which it may never plan.
    for sql in [
        "SELECT 1 FROM (VALUES (1 / 0)) v",
        "SELECT k FROM s, (VALUES (1 / 0)) v",
        "SELECT k FROM s, LATERAL (SELECT 2147483647 + 1 AS z) l",
        "WITH c AS (SELECT 1 / 0 AS q, 1 AS r) SELECT r FROM c",
        "WITH c AS (SELECT 1 / 0 AS q) SELECT 1 FROM c",
        "WITH c AS MATERIALIZED (SELECT 1 / 0 AS q) SELECT 1",
        "WITH c AS (SELECT 1 / 0 AS q) SELECT CASE WHEN true THEN 1 ELSE (SELECT q FROM c) END",
        "SELECT CASE WHEN true THEN 1 ELSE x.q END FROM (SELECT 1 / 0 AS q) x",
        "SELECT x.q FROM (SELECT 1 / 0 AS q LIMIT 1) x WHERE false",
        "SELECT count(*) FROM (SELECT 1 / 0 AS q) x",
        "SELECT 1 FROM (SELECT y.q FROM (SELECT 1 / 0 AS q) y) x",
        "SELECT x.k FROM (SELECT 1 / 0 AS q, k FROM s) x",
        // A WINDOW clause no call uses keeps no subquery from being pulled
        // up (is_simple_subquery checks hasWindowFuncs), and is dropped.
        "SELECT x.k FROM (SELECT k FROM s WINDOW w AS (PARTITION BY 1 / 0)) x",
        "SELECT x.k FROM (SELECT k FROM s WINDOW w AS (ORDER BY 1 / 0)) x WHERE false",
        "SELECT x.k FROM s LEFT JOIN (SELECT k FROM s WINDOW w AS (PARTITION BY 1 / 0)) x ON true",
        "SELECT x.k FROM (SELECT k FROM s WINDOW w AS (ROWS 2147483647 + 1 PRECEDING)) x",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
}

#[test]
fn constants_a_pulled_up_subquery_substitutes_are_folded() {
    // `pullup_replace_vars` puts a pulled-up subquery's output in place of
    // the outer query's reference before `eval_const_expressions` runs: a
    // cast or operator over it folds as over the constant itself.
    let db = folding_setup();
    for (sql, msg) in [
        (
            "SELECT x::int2 FROM (SELECT 40000 AS x) q",
            "smallint out of range",
        ),
        (
            "SELECT q.x::int2 FROM s, (SELECT 40000 AS x) q",
            "smallint out of range",
        ),
        (
            "SELECT x::int2 FROM (SELECT 40000 AS x) q WHERE false",
            "smallint out of range",
        ),
        (
            "SELECT 1 FROM (SELECT 40000 AS x) q WHERE x::int2 = 1",
            "smallint out of range",
        ),
        (
            "SELECT x::int2 FROM (SELECT 40000 AS x) q GROUP BY x",
            "smallint out of range",
        ),
        (
            "SELECT (SELECT x::int2) FROM (SELECT 40000 AS x) q",
            "smallint out of range",
        ),
        (
            "SELECT (x + 1)::int2 FROM (SELECT 32767 AS x) q",
            "smallint out of range",
        ),
        (
            "SELECT x::int2 FROM (VALUES (40000)) v(x)",
            "smallint out of range",
        ),
        (
            "SELECT y::int2 FROM (SELECT x AS y FROM (SELECT 40000 AS x) a) b",
            "smallint out of range",
        ),
        (
            "SELECT x + 2147483647 FROM (SELECT 1 AS x) q",
            "integer out of range",
        ),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::NumericValueOutOfRange(_),
            msg
        );
    }
    assert_err_prefix!(
        db.analyze("SELECT x::numeric(3,1) FROM (SELECT 100 AS x) q"),
        AnalyzeError::NumericValueOutOfRange(_),
        "numeric field overflow"
    );
    assert_err_prefix!(
        db.analyze("SELECT x / 0 FROM (SELECT 1 AS x) q"),
        AnalyzeError::DivisionByZero(_),
        "division by zero"
    );
    // A PlaceHolderVar keeps the nullable side's output from folding; a
    // subquery planned on its own keeps its output a Var; a branch never
    // folded is never reached.
    for sql in [
        "SELECT q.x::int2 FROM s LEFT JOIN (SELECT 40000 AS x) q ON true",
        "SELECT x::int2 FROM (SELECT 40000 AS x FROM s LIMIT 1) q",
        "SELECT CASE WHEN false THEN x::int2 END FROM (SELECT 40000 AS x) q",
        "SELECT x::int2 FROM (SELECT 4000 AS x) q",
        "SELECT x::int2 FROM (SELECT b AS x FROM s) q",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
}

#[test]
fn text_constants_converted_through_io_are_folded() {
    // A cast from `text` without a cast function is a conversion through
    // the types' I/O functions (`CoerceViaIO`), which the planner runs on
    // a constant when they are immutable: the target's input function
    // fails every execution as it fails an untyped literal at parse time.
    let db = folding_setup();
    for (sql, msg) in [
        (
            "SELECT '40000'::text::int2",
            "value \"40000\" is out of range for type smallint",
        ),
        (
            "SELECT 'abc'::text::int4",
            "invalid input syntax for type integer: \"abc\"",
        ),
        (
            "SELECT 'x'::varchar::int",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "SELECT 'x'::text::bool",
            "invalid input syntax for type boolean: \"x\"",
        ),
        (
            "SELECT 'x'::text::numeric",
            "invalid input syntax for type numeric: \"x\"",
        ),
        (
            "SELECT 'x'::text::float8",
            "invalid input syntax for type double precision: \"x\"",
        ),
        (
            "SELECT '1e400'::text::float8",
            "\"1e400\" is out of range for type double precision",
        ),
        // A pulled-up subquery's untyped literal output is a text constant.
        (
            "SELECT x::int4 FROM (SELECT 'abc' AS x) q",
            "invalid input syntax for type integer: \"abc\"",
        ),
        (
            "SELECT x::int2 FROM (SELECT '40000'::text AS x) q",
            "value \"40000\" is out of range for type smallint",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::InvalidLiteral(_), msg);
    }
    for sql in [
        "SELECT ' 12 '::text::int8",
        "SELECT CASE WHEN false THEN 'x'::text::int END",
        "SELECT q.x::int4 FROM s LEFT JOIN (SELECT 'abc' AS x) q ON true",
        "SELECT x::int4 FROM (SELECT 'abc' AS x FROM s LIMIT 1) q",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
}

#[test]
fn a_view_output_fails_where_it_is_read() {
    // The rewriter puts a view's query in the statement reading it, and
    // the planner pulls it up like a FROM subquery — unless it is a
    // security barrier, or limited: then it plans it on its own, folding
    // what is read of it (`remove_unused_subquery_outputs` drops the
    // rest), unless the statement proves it empty first.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, x int);
         CREATE VIEW consts AS SELECT 40000::int2 AS a, 1 AS b;
         CREATE VIEW cols AS SELECT x, 1 / 0 AS z FROM t;
         CREATE VIEW filtered AS SELECT x FROM t WHERE 1 / 0 = 1;
         CREATE VIEW nested AS SELECT a FROM consts;
         CREATE VIEW nested_ok AS SELECT b FROM consts;
         CREATE VIEW barrier WITH (security_barrier) AS SELECT x, 1 / 0 AS z FROM t;
         CREATE VIEW barrier_reset WITH (security_barrier) AS SELECT x, 1 / 0 AS z FROM t;
         ALTER VIEW barrier_reset RESET (security_barrier);
         CREATE VIEW limited AS SELECT x, 1 / 0 AS z FROM t LIMIT 5;
         CREATE VIEW barrier_where WITH (security_barrier) AS SELECT x FROM t WHERE 1 / 0 = 1;
         CREATE VIEW barrier_consts WITH (security_barrier) AS SELECT 40000::int2 AS a, 1 AS b;
         CREATE VIEW grouped AS SELECT x, count(*) AS c FROM t WHERE 1 / 0 = 1 GROUP BY x;",
    )
    .unwrap();
    for sql in [
        "SELECT a FROM consts",
        "SELECT a FROM nested",
        "SELECT a FROM barrier_consts",
        "SELECT a FROM (SELECT 40000::int2 AS a, 1 AS b OFFSET 0) s",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::NumericValueOutOfRange(_),
            "smallint out of range"
        );
    }
    for sql in [
        "SELECT z FROM cols",
        "SELECT * FROM cols",
        "SELECT cols FROM cols",
        "SELECT x FROM filtered",
        "SELECT 1 FROM filtered",
        "SELECT z FROM barrier_reset",
        "SELECT z FROM barrier",
        "SELECT z FROM limited",
        "SELECT 1 FROM barrier_where",
        "SELECT 1 FROM grouped",
        "SELECT 1 FROM (SELECT x FROM t WHERE 1 / 0 = 1 OFFSET 0) s",
        "SELECT 1 FROM t, barrier_where WHERE false",
        // An aggregate, window, set-returning or volatile call keeps a
        // subquery from being pulled up too.
        "SELECT c FROM (SELECT sum(1 / 0) AS c FROM t) s",
        "SELECT 1 FROM (SELECT count(*) AS c FROM t WHERE 1 / 0 = 1) s",
        "SELECT 1 FROM (SELECT random() AS r FROM t WHERE 1 / 0 = 1) s",
        "SELECT z FROM (SELECT random() AS r, 1 / 0 AS z FROM t) s",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DivisionByZero(_),
            "division by zero"
        );
    }
    for sql in [
        "SELECT b FROM consts",
        "SELECT 1 FROM consts",
        "SELECT x FROM cols",
        "SELECT b FROM nested_ok",
        "SELECT x FROM barrier",
        "SELECT x FROM limited",
        "SELECT z FROM barrier WHERE false",
        "SELECT z FROM limited WHERE false",
        "SELECT b FROM barrier_consts",
        "SELECT b FROM (SELECT 40000::int2 AS a, 1 AS b OFFSET 0) s",
        "SELECT 1 FROM barrier_where WHERE false",
        "SELECT 1 FROM grouped WHERE false",
        "SELECT 1 FROM (SELECT count(*) AS c FROM t WHERE 1 / 0 = 1) s WHERE false",
        "SELECT 1 FROM (SELECT random() AS r, 1 / 0 AS z FROM t) s",
        // The nullable side of a join the planner may remove.
        "SELECT 1 FROM t LEFT JOIN (SELECT count(*) AS c FROM t WHERE 1 / 0 = 1) s ON true",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
}

#[test]
fn a_rule_action_fails_the_statements_firing_it() {
    // PG stores the action; every statement firing the rule plans it —
    // whatever its condition — unless an INSTEAD rule replaces it in turn.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int, a int2, b int4);
         CREATE TABLE u (id int);
         CREATE RULE r_u AS ON INSERT TO u DO ALSO INSERT INTO t (id, a) VALUES (1, 40000);
         CREATE TABLE w (id int, a int);
         CREATE VIEW wv AS SELECT * FROM w;
         CREATE RULE r_wv AS ON UPDATE TO wv DO INSTEAD
             INSERT INTO t (id, b) VALUES (1, 3000000000);
         CREATE TABLE c (id int);
         CREATE RULE r_c AS ON INSERT TO c WHERE NEW.id > 100 DO ALSO
             INSERT INTO t (id, a) VALUES (1, 40000);
         CREATE TABLE cf (id int);
         CREATE RULE r_cf AS ON INSERT TO cf WHERE false DO ALSO
             INSERT INTO t (id, a) VALUES (1, 40000);
         CREATE TABLE sel (id int);
         CREATE RULE r_sel AS ON INSERT TO sel DO ALSO SELECT 1 / 0;
         CREATE TABLE n (id int);
         CREATE RULE r_n AS ON INSERT TO n DO ALSO INSERT INTO u (id) VALUES (2147483647 + 1);
         CREATE TABLE nothing (id int, a int2);
         CREATE RULE r_nothing AS ON INSERT TO nothing DO INSTEAD NOTHING;
         CREATE TABLE m (id int);
         CREATE RULE r_m AS ON INSERT TO m DO ALSO INSERT INTO nothing (id, a) VALUES (1, 40000);
         CREATE TABLE nw (id int);
         CREATE RULE r_nw AS ON INSERT TO nw DO ALSO
             INSERT INTO t (id, a) VALUES (NEW.id, CASE WHEN NEW.id > 0 THEN 40000 ELSE 1 END);",
    )
    .unwrap();
    for (sql, msg) in [
        ("INSERT INTO u (id) VALUES (1)", "smallint out of range"),
        ("UPDATE wv SET a = 5", "integer out of range"),
        ("INSERT INTO c (id) VALUES (1)", "smallint out of range"),
        ("INSERT INTO cf (id) VALUES (1)", "smallint out of range"),
        ("INSERT INTO n (id) VALUES (1)", "integer out of range"),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::NumericValueOutOfRange(_),
            msg
        );
    }
    assert_err_prefix!(
        db.analyze("INSERT INTO sel (id) VALUES (1)"),
        AnalyzeError::DivisionByZero(_),
        "division by zero"
    );
    for sql in [
        // Replaced by an INSTEAD rule on its own target.
        "INSERT INTO m (id) VALUES (1)",
        // NEW stands for the statement's value: the CASE may fold.
        "INSERT INTO nw (id) VALUES (-1)",
        // Other events fire no rule.
        "DELETE FROM u",
        "UPDATE u SET id = 1",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
}

#[test]
fn a_default_the_planner_folds_fails_the_statements_writing_it() {
    // PG stores the default; every statement it is written by fails while
    // planned, rows or not.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE d (k int PRIMARY KEY, a int2 DEFAULT 40000, b int);
         CREATE TABLE dv (k int, v varchar(3) DEFAULT 'abcd');
         CREATE TABLE dz (k int, z int DEFAULT 1 / 0);",
    )
    .unwrap();
    for sql in [
        "INSERT INTO d (k) VALUES (1)",
        "INSERT INTO d DEFAULT VALUES",
        "INSERT INTO d (k, a) VALUES (1, 1), (2, DEFAULT)",
        "INSERT INTO d (k) SELECT 1 WHERE false",
        "INSERT INTO d (k) VALUES (1) ON CONFLICT DO NOTHING",
        "UPDATE d SET a = DEFAULT WHERE false",
        "MERGE INTO d USING (SELECT 1 AS k) x ON d.k = x.k \
         WHEN NOT MATCHED THEN INSERT (k) VALUES (1)",
        "MERGE INTO d USING (SELECT 1 AS k) x ON d.k = x.k \
         WHEN NOT MATCHED THEN INSERT DEFAULT VALUES",
        "MERGE INTO d USING (SELECT 1 AS k) x ON d.k = x.k \
         WHEN MATCHED THEN UPDATE SET a = DEFAULT",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::NumericValueOutOfRange(_),
            "smallint out of range"
        );
    }
    assert_err_prefix!(
        db.analyze("INSERT INTO dv (k) VALUES (1)"),
        AnalyzeError::Invalid(_),
        "value too long for type character varying(3)"
    );
    assert_err_prefix!(
        db.analyze("INSERT INTO dz (k) VALUES (1)"),
        AnalyzeError::DivisionByZero(_),
        "division by zero"
    );
    // Given a value, the default isn't used.
    for sql in [
        "INSERT INTO d (k, a) VALUES (1, 1)",
        "UPDATE d SET a = 1",
        "MERGE INTO d USING (SELECT 1 AS k) x ON d.k = x.k \
         WHEN NOT MATCHED THEN INSERT (k, a) VALUES (1, 2)",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
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
        concat!(
            "cannot cast type date to boolean\n",
            "  ╭────\n",
            "1 │ SELECT '2020-01-01'::date::bool\n",
            "  ·        ────────────\n",
            "  ╰────\n",
        ),
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

/// `find_coercion_pathway` coerces arrays element-wise when their element
/// types coerce (`ArrayCoerceExpr`), so common-type constructs over
/// `integer[]` and `numeric[]` resolve to `numeric[]` and an assignment
/// may narrow `numeric[]` into an `integer[]` column.
#[test]
fn arrays_coerce_element_wise() {
    let mut db = setup();
    db.apply_sql("CREATE TABLE ta (id INT PRIMARY KEY, arr INT[]);")
        .unwrap();
    for sql in [
        "SELECT COALESCE(ARRAY[1], ARRAY[2.5]) AS a",
        "SELECT CASE WHEN true THEN ARRAY[1] ELSE ARRAY[2.5] END AS a",
        "SELECT ARRAY[1] AS a UNION SELECT ARRAY[2.5]",
        "SELECT GREATEST(ARRAY[1], ARRAY[2.5]) AS a",
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        // (Only the UNION's arms carry their elements' nullability.)
        let element_nullable = sql.contains("UNION").then_some(false);
        let expected = Type::Array {
            element: Box::new(numeric()),
            element_nullable,
        };
        assert_eq!(s.columns[0].pg_type, expected, "{sql}");
    }
    for sql in [
        "SELECT ARRAY[ARRAY[1], ARRAY[2.5]] AS a",
        "SELECT ARRAY[[1], [2.5]] AS a",
        "SELECT ARRAY[arr, ARRAY[1.5]] AS a FROM ta",
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(s.columns[0].pg_type, array_of(numeric()), "{sql}");
    }
    let s = db
        .analyze("SELECT ARRAY[ARRAY[1]::int[], ARRAY[2]::bigint[]] AS a")
        .unwrap();
    assert_cols(&s, vec![c("a", array_of(int8()))]);
    db.analyze("INSERT INTO ta (id, arr) VALUES (1, ARRAY[1.5])")
        .unwrap();
    db.analyze("UPDATE ta SET arr = ARRAY[1.5]::numeric[]")
        .unwrap();
}

/// gram.y rejects an out-of-range `float(p)` precision itself, with
/// `invalid_parameter_value` rather than a syntax error.
#[test]
fn float_precision_out_of_range_is_a_grammar_error() {
    let db = setup();
    assert_err_prefix!(
        db.analyze("SELECT 1::float(0)"),
        AnalyzeError::Invalid(_),
        "precision for type float must be at least 1 bit"
    );
    assert_err_prefix!(
        db.analyze("SELECT 1::float(60)"),
        AnalyzeError::Invalid(_),
        "precision for type float must be less than 54 bits"
    );
}

/// can_coerce_type's pseudo-type targets: `"any"` and a polymorphic
/// pseudo-type take a value consistent with them, and coerce_type leaves it
/// as it is (an array, enum or range target strips a domain); any other
/// pseudo-type is reachable only through pg_cast or the I/O rule.
#[test]
fn casts_to_pseudo_types_follow_can_coerce_type() {
    let mut db = setup();
    db.apply_sql(
        "CREATE DOMAIN posint AS int CHECK (VALUE > 0);
         CREATE DOMAIN ia AS int[];
         CREATE TABLE tv (v varchar(3) NOT NULL, n numeric(4, 1));",
    )
    .unwrap();
    for (sql, expected) in [
        ("SELECT 1::anyelement AS a", int4()),
        ("SELECT 1::anynonarray AS a", int4()),
        ("SELECT 1::anycompatible AS a", int4()),
        ("SELECT 1::anycompatiblenonarray AS a", int4()),
        ("SELECT 1::\"any\" AS a", int4()),
        (
            "SELECT ARRAY[1]::anyelement AS a",
            array_with_elems(int4(), false),
        ),
        (
            "SELECT ARRAY[1]::anyarray AS a",
            array_with_elems(int4(), false),
        ),
        (
            "SELECT ARRAY[1]::anycompatiblearray AS a",
            array_with_elems(int4(), false),
        ),
        (
            "SELECT ARRAY[1]::ia::anyarray AS a",
            array_with_elems(int4(), false),
        ),
        ("SELECT v::text::anyelement AS a FROM tv", text()),
        // coerce_type_typmod relabels a value with a typmod to the target.
        (
            "SELECT v::anyelement AS a FROM tv",
            basic("pg_catalog", "anyelement"),
        ),
        (
            "SELECT n::anycompatible AS a FROM tv",
            basic("pg_catalog", "anycompatible"),
        ),
        ("SELECT v::\"any\" AS a FROM tv", basic("pg_catalog", "any")),
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(s.columns[0].pg_type, expected, "{sql}");
    }
    // The value keeps its type: a range, a domain (but an array, enum or
    // range target strips it).
    let ty = |sql: &str| {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .columns[0]
            .pg_type
            .clone()
    };
    for (sql, same_as) in [
        (
            "SELECT '[1,2]'::int4range::anyrange AS a",
            "SELECT '[1,2]'::int4range AS a",
        ),
        (
            "SELECT '[1,2]'::int4range::anycompatiblerange AS a",
            "SELECT '[1,2]'::int4range AS a",
        ),
        (
            "SELECT '{[1,2]}'::int4multirange::anymultirange AS a",
            "SELECT '{[1,2]}'::int4multirange AS a",
        ),
        ("SELECT 1::posint::anyelement AS a", "SELECT 1::posint AS a"),
    ] {
        assert_eq!(ty(sql), ty(same_as), "{sql}");
    }
    for (sql, message) in [
        ("SELECT 1::anyarray", "cannot cast type integer to anyarray"),
        ("SELECT 1::anyenum", "cannot cast type integer to anyenum"),
        ("SELECT 1::anyrange", "cannot cast type integer to anyrange"),
        (
            "SELECT 1::anymultirange",
            "cannot cast type integer to anymultirange",
        ),
        (
            "SELECT 1::anycompatiblemultirange",
            "cannot cast type integer to anycompatiblemultirange",
        ),
        (
            "SELECT ARRAY[1]::anynonarray",
            "cannot cast type integer[] to anynonarray",
        ),
        (
            "SELECT ARRAY[1]::anymultirange",
            "cannot cast type integer[] to anymultirange",
        ),
        (
            "SELECT 'a'::varchar::anyarray",
            "cannot cast type character varying to anyarray",
        ),
        ("SELECT 1::cstring", "cannot cast type integer to cstring"),
        ("SELECT 1::void", "cannot cast type integer to void"),
        ("SELECT 1::internal", "cannot cast type integer to internal"),
        ("SELECT 1::trigger", "cannot cast type integer to trigger"),
        ("SELECT 1::record", "cannot cast type integer to record"),
        (
            "SELECT 'a'::cstring::int",
            "cannot cast type cstring to integer",
        ),
        (
            "SELECT pg_sleep(0)::int",
            "cannot cast type void to integer",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::Invalid(_), message);
    }
    // The I/O rule reaches a pseudo-type from a string type and back.
    for sql in [
        "SELECT 'a'::text::cstring AS a",
        "SELECT pg_sleep(0)::text AS a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // `unknown` and `internal` take no cast but their own; `anyenum` no
    // untyped value.
    for (sql, message) in [
        ("SELECT 1::unknown", "cannot cast type integer to unknown"),
        (
            "SELECT ROW(1, 2)::unknown",
            "cannot cast type record to unknown",
        ),
        (
            "SELECT 'a'::internal",
            "cannot cast type unknown to internal",
        ),
        (
            "SELECT NULL::internal",
            "cannot cast type unknown to internal",
        ),
        (
            "SELECT $1::internal",
            "cannot cast type unknown to internal",
        ),
        (
            "SELECT 'a'::text::internal",
            "cannot cast type text to internal",
        ),
        ("SELECT 'a'::anyenum", "cannot cast type unknown to anyenum"),
        (
            "SELECT NULL::anyenum",
            "cannot cast type unknown to anyenum",
        ),
        ("SELECT $1::anyenum", "cannot cast type unknown to anyenum"),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::Invalid(_), message);
    }
}

/// An untyped literal or parameter cast to `"any"`, `anyelement`,
/// `anynonarray`, `anycompatible` or `anycompatiblenonarray` is handed back
/// untouched by coerce_type: it is still untyped, and resolves as if the
/// cast weren't there (an output column to `text`).
#[test]
fn untyped_operand_passes_through_a_polymorphic_cast() {
    let db = setup();
    for target in [
        "\"any\"",
        "anyelement",
        "anynonarray",
        "anycompatible",
        "anycompatiblenonarray",
    ] {
        for sql in [
            format!("SELECT 'a'::{target} AS a"),
            format!("SELECT NULL::{target} AS a"),
            format!("SELECT x FROM (SELECT 'a'::{target} AS x) s"),
            format!("SELECT ('a'::{target})::anyelement AS a"),
        ] {
            let s = db.analyze(&sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert_eq!(s.columns[0].pg_type, text(), "{sql}");
        }
        let sql = format!("SELECT $1::{target} AS a");
        let s = db.analyze(&sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(s.columns[0].pg_type, text(), "{sql}");
        assert_eq!(s.params[0].pg_type, text(), "{sql}");
    }
    // As an operator's operand it is an unknown one.
    let s = db.analyze("SELECT $1::anyelement || 'x' AS a").unwrap();
    assert_eq!(s.params[0].pg_type, text());
    // The literal's content is checked by the outer cast.
    assert_err_prefix!(
        db.analyze("SELECT ('a'::anyelement)::int"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"a\""
    );
    // Nothing gives the parameter a type.
    assert_err_prefix!(
        db.analyze("SELECT 1 AS a WHERE $1::anyelement IS NULL"),
        AnalyzeError::IndeterminateType(_),
        "could not determine data type of parameter $1"
    );
    // A bare output parameter is coerced to text, which conflicts with a
    // type a later clause deduced.
    assert_err_prefix!(
        db.analyze("SELECT $1::anyelement AS a, $1 = 1 AS b"),
        AnalyzeError::AmbiguousParameter(_),
        "inconsistent types deduced for parameter $1"
    );
}

/// stringTypeDatum runs the target's input function on an untyped literal:
/// a pseudo-type whose input function is a dummy refuses every value, and
/// a NULL one too when that function isn't strict. A parameter is never
/// read at parse time.
#[test]
fn literal_of_a_pseudo_type_without_input() {
    let db = setup();
    for (target, strict) in [
        ("anyarray", true),
        ("anyrange", true),
        ("anymultirange", true),
        ("anycompatiblearray", true),
        ("anycompatiblerange", true),
        ("anycompatiblemultirange", true),
        ("trigger", false),
        ("event_trigger", false),
        ("language_handler", false),
        ("fdw_handler", false),
        ("table_am_handler", false),
        ("index_am_handler", false),
        ("tsm_handler", false),
    ] {
        let message = format!("cannot accept a value of type {target}");
        assert_err_prefix!(
            db.analyze(&format!("SELECT 'a'::{target}")),
            AnalyzeError::FeatureNotSupported(_),
            &message
        );
        let null = format!("SELECT NULL::{target} AS a");
        if strict {
            let s = db.analyze(&null).unwrap_or_else(|e| panic!("{null}: {e}"));
            assert_eq!(s.columns[0].pg_type, basic("pg_catalog", target), "{null}");
        } else {
            assert_err_prefix!(
                db.analyze(&null),
                AnalyzeError::FeatureNotSupported(_),
                &message
            );
        }
        let param = format!("SELECT $1::{target} AS a");
        let s = db
            .analyze(&param)
            .unwrap_or_else(|e| panic!("{param}: {e}"));
        assert_eq!(s.params[0].pg_type, basic("pg_catalog", target), "{param}");
    }
    // Pseudo-types with a real input function.
    for sql in ["SELECT 'a'::cstring AS a", "SELECT 'a'::void AS a"] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// A value cast to `unknown` (a CoerceViaIO, no literal) is not what an
/// untyped literal is: PG's coerce_type reads a literal or a parameter as
/// any type, but finds no conversion for such a value except the I/O one to
/// a string type, on assignment or by an explicit cast. Every implicit use
/// — an output column resolved as text, an operator or function argument,
/// a branch of CASE, an element, a sort key, a clause's boolean — fails to
/// prepare (XX000).
#[test]
fn a_value_cast_to_unknown_is_no_literal() {
    let mut db = setup();
    db.apply_sql("CREATE DOMAIN short_text AS text; CREATE TABLE notes (n short_text);")
        .unwrap();
    for (sql, target) in [
        ("SELECT 'a'::text::unknown AS a", "text"),
        ("SELECT NULL::text::unknown AS a", "text"),
        ("SELECT name::unknown AS a FROM users", "text"),
        ("SELECT 'a'::varchar::unknown AS a", "text"),
        ("SELECT ('a'::text::unknown)::unknown AS a", "text"),
        ("SELECT ('a'::text::unknown) COLLATE \"C\" AS a", "text"),
        ("SELECT unknownin('a') AS a", "text"),
        ("SELECT x FROM (SELECT 'a'::text::unknown AS x) s", "text"),
        (
            "WITH c AS (SELECT 'a'::text::unknown AS x) SELECT 1 AS a FROM c",
            "text",
        ),
        ("SELECT EXISTS (SELECT 'a'::text::unknown) AS a", "text"),
        ("SELECT (SELECT 'a'::text::unknown) AS a", "text"),
        ("SELECT ARRAY(SELECT 'a'::text::unknown) AS a", "text"),
        (
            "SELECT 1 AS a FROM users WHERE name IN (SELECT 'a'::text::unknown)",
            "text",
        ),
        ("VALUES ('a'::text::unknown)", "text"),
        ("VALUES ('a'::text::unknown), ('b')", "text"),
        ("VALUES (1), ('a'::text::unknown)", "integer"),
        (
            "SELECT 1 AS a FROM users WHERE name = 'a'::text::unknown",
            "text",
        ),
        (
            "SELECT 1 AS a FROM users WHERE age = 'a'::text::unknown",
            "integer",
        ),
        ("SELECT 'a'::text::unknown = 'b' AS a", "text"),
        ("SELECT 'a'::text::unknown || 'b' AS a", "text"),
        ("SELECT 'a'::text::unknown BETWEEN 'a' AND 'b' AS a", "text"),
        (
            "SELECT 'a'::text::unknown IS DISTINCT FROM 'b' AS a",
            "text",
        ),
        (
            "SELECT 1 AS a FROM users WHERE 'x'::text::unknown IN ('a', 'b')",
            "text",
        ),
        ("SELECT ('a'::text::unknown, 1) = ('a', 1) AS a", "text"),
        ("SELECT length('a'::text::unknown) AS a", "text"),
        ("SELECT max('a'::text::unknown) AS a", "text"),
        (
            "SELECT array_append(ARRAY['x'], 'a'::text::unknown) AS a",
            "text",
        ),
        ("SELECT coalesce('a'::text::unknown, 'b') AS a", "text"),
        ("SELECT NULLIF('a'::text::unknown, 'b') AS a", "text"),
        ("SELECT GREATEST('a'::text::unknown, 'b') AS a", "text"),
        (
            "SELECT CASE WHEN true THEN 'a'::text::unknown END AS a",
            "text",
        ),
        (
            "SELECT CASE 'a'::text::unknown WHEN 'a' THEN 1 END AS a",
            "text",
        ),
        ("SELECT ARRAY['a'::text::unknown] AS a", "text"),
        ("SELECT 'a'::text::unknown IS DOCUMENT AS a", "xml"),
        (
            "SELECT 1 AS a FROM users WHERE 'a'::text::unknown",
            "boolean",
        ),
        (
            "SELECT 1 AS a FROM users LIMIT 'a'::text::unknown",
            "bigint",
        ),
        (
            "SELECT count(*) AS a FROM users GROUP BY 'a'::text::unknown",
            "text",
        ),
        ("SELECT name FROM users ORDER BY 'a'::text::unknown", "text"),
        (
            "SELECT DISTINCT ON ('a'::text::unknown) name FROM users",
            "text",
        ),
        ("SELECT count(DISTINCT 'a'::text::unknown) AS a", "text"),
        (
            "SELECT string_agg('x', ',' ORDER BY 'a'::text::unknown) AS a",
            "text",
        ),
        (
            "SELECT row_number() OVER (PARTITION BY 'a'::text::unknown) AS a",
            "text",
        ),
        // Explicit casts: through I/O to a string type only.
        ("SELECT 'a'::text::unknown::int AS a", "integer"),
        ("SELECT 'a'::text::unknown::json AS a", "json"),
        ("SELECT 'a'::text::unknown::\"char\" AS a", "\"char\""),
        // An `ARRAY[…]` cast to an array type coerces its elements to the
        // element type explicitly.
        ("SELECT ARRAY['a'::text::unknown]::int[] AS a", "integer"),
        ("SELECT ARRAY['a'::text::unknown, 'b']::json[] AS a", "json"),
        (
            "SELECT ARRAY[ARRAY[name::unknown]]::int[] AS a FROM users",
            "integer",
        ),
        // Assignment: likewise.
        (
            "INSERT INTO users (id, age) VALUES (1, 'a'::text::unknown)",
            "integer",
        ),
        (
            "INSERT INTO users (id, age) SELECT 1, 'a'::text::unknown",
            "integer",
        ),
        ("UPDATE users SET age = 'a'::text::unknown", "integer"),
        // An INSERT's SELECT keeps its unknowns, not the levels below it, nor
        // its sort keys.
        (
            "INSERT INTO users (id, name) SELECT 1, x FROM (SELECT 'a'::text::unknown AS x) s",
            "text",
        ),
        (
            "INSERT INTO users (id, name) SELECT 1, 'a'::text::unknown ORDER BY 2",
            "text",
        ),
        (
            "UPDATE users SET (name) = (SELECT 'a'::text::unknown)",
            "text",
        ),
        (
            "INSERT INTO users (id, name) VALUES (1, 'a') RETURNING 'a'::text::unknown",
            "text",
        ),
        (
            "WITH w AS (DELETE FROM users RETURNING name::unknown AS x) SELECT 1 AS a",
            "text",
        ),
        // A set operation's arm keeps them too, but not its own ORDER BY.
        (
            "(SELECT 'a'::text::unknown ORDER BY 1) UNION SELECT 'b'",
            "text",
        ),
    ] {
        let message = format!("failed to find conversion function from unknown to {target}");
        assert_err_prefix!(db.analyze(sql), AnalyzeError::PgInternalError(_), &message);
    }
    for sql in [
        // A literal cast to `unknown` is still a literal.
        "SELECT 'a'::unknown AS a",
        "SELECT 'a'::unknown::unknown AS a",
        "SELECT 1 AS a FROM users WHERE age = '1'::unknown::unknown::text::int",
        // No coercion: IS NULL, `"any"` arguments, a row's field.
        "SELECT 'a'::text::unknown IS NULL AS a",
        "SELECT pg_typeof('a'::text::unknown) AS a",
        "SELECT count('a'::text::unknown) AS a",
        "SELECT format('%s', 'a'::text::unknown) AS a",
        "SELECT concat('a'::text::unknown) AS a",
        "SELECT num_nulls('a'::text::unknown) AS a",
        "SELECT jsonb_build_object('k', 'a'::text::unknown) AS a",
        "SELECT ROW('a'::text::unknown) AS a",
        "SELECT ROW('a'::text::unknown)::text AS a",
        "SELECT 'a'::text::unknown::anyelement IS NULL AS a",
        // Explicit and assignment casts to a string type (or a domain over
        // one).
        "SELECT 'a'::text::unknown::text AS a",
        "SELECT ('a'::text::unknown)::varchar(1) AS a",
        "SELECT 'a'::text::unknown::name AS a",
        "SELECT 'a'::text::unknown::bpchar AS a",
        "SELECT 'a'::text::unknown::short_text AS a",
        // ... and so are the elements of an `ARRAY[…]` cast to an array of
        // one (transformArrayExpr coerces them with COERCION_EXPLICIT).
        "SELECT ARRAY['a'::text::unknown]::text[] AS a",
        "SELECT ARRAY['a'::text::unknown, 'b']::text[] AS a",
        "SELECT CAST(ARRAY['a'::text::unknown] AS text[]) AS a",
        "SELECT ARRAY[name::unknown]::varchar[] AS a FROM users",
        "SELECT ARRAY['a'::text::unknown]::name[] AS a",
        "SELECT ARRAY[(ROW('x')).f1]::text[] AS a",
        "SELECT ARRAY[ARRAY[name::unknown], ARRAY['b']]::text[] AS a FROM users",
        "SELECT ARRAY['a'::text::unknown]::short_text[] AS a",
        "SELECT 'a' = ANY(ARRAY['a'::text::unknown]::text[]) AS a",
        "INSERT INTO users (id, name) VALUES (1, 'a'::text::unknown)",
        "INSERT INTO users (id, name) VALUES (1, 'a'::text::unknown), (2, 'b'::text::unknown)",
        "INSERT INTO users (id, name) SELECT 1, 'a'::text::unknown",
        "INSERT INTO users (id, name) SELECT 1, 'a'::text::unknown UNION SELECT 2, 'b'",
        "INSERT INTO notes (n) VALUES ('a'::text::unknown)",
        "INSERT INTO users (id, name) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET name = 'z'::text::unknown",
        "UPDATE users SET name = 'a'::text::unknown",
        "UPDATE users SET (age, name) = (1, 'a'::text::unknown)",
        "MERGE INTO users u USING (SELECT 1 AS k) s ON u.id = s.k \
         WHEN MATCHED THEN UPDATE SET name = 'a'::text::unknown \
         WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.k, 'a'::text::unknown)",
        // A set operation's arm is not coerced at parse time (the planner
        // may fold the value to a constant first).
        "SELECT 'a'::text::unknown AS a UNION SELECT 'b'",
        "SELECT 'a'::text::unknown AS a UNION SELECT 'b'::text::unknown",
        "SELECT 1 AS a UNION SELECT name::unknown FROM users",
        "SELECT 'a'::text::unknown AS a INTERSECT SELECT 1",
        "SELECT x FROM (SELECT 'a'::text::unknown UNION SELECT 'b'::text) s(x)",
        "WITH RECURSIVE c(x) AS (SELECT 'a'::text::unknown UNION ALL SELECT x FROM c WHERE false) \
         SELECT x FROM c",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // The set operation's column is select_common_type's.
    for (sql, expected) in [
        ("SELECT 'a'::text::unknown AS a UNION SELECT 'b'", text()),
        ("SELECT 'a'::text::unknown AS a UNION SELECT 1", int4()),
        ("SELECT 'a'::text::unknown::name AS a", name_ty()),
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(s.columns[0].pg_type, expected, "{sql}");
    }
}
