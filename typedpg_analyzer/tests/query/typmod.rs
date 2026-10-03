//! `pg_attribute.atttypmod` / `pg_type.typtypmod` propagation.
//!
//! These exercise the analyzer's modeling of parametric type modifiers:
//! `varchar(n)`, `numeric(p, s)`, `time(p)`, pgvector's `vector(N)`, and the
//! way they flow through column refs, casts, CASE/COALESCE/UNION, domain
//! inheritance, and `ALTER TABLE … ALTER COLUMN TYPE`.

use crate::common::*;

// ── varchar / numeric basics ──────────────────────────────────────────────

#[test]
fn varchar_typmod_propagates_to_select() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(50) NOT NULL);")
        .unwrap();
    let s = db.analyze("SELECT name FROM t").unwrap();
    // varchar(50) → typmod = 50 + 4 = 54.
    assert_cols(&s, vec![c("name", varchar_n(50))]);
}

#[test]
fn numeric_typmod_propagates_to_select() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, price NUMERIC(10, 2) NOT NULL);")
        .unwrap();
    let s = db.analyze("SELECT price FROM t").unwrap();
    assert_cols(&s, vec![c("price", numeric_ps(10, 2))]);
}

#[test]
fn varchar_typmod_within_bounds_is_accepted() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (slug VARCHAR(8) NOT NULL);")
        .unwrap();
    db.analyze("INSERT INTO t (slug) VALUES ('hi')").unwrap();
}

#[test]
fn numeric_typmod_within_bounds_is_accepted() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, amount NUMERIC(4,2) NOT NULL);")
        .unwrap();
    db.analyze("INSERT INTO t (id, amount) VALUES ($p1, 12.34)")
        .unwrap();
}

// ── pgvector — the headline use case ──────────────────────────────────────

#[test]
fn vector_dimension_propagates_to_select() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE EXTENSION vector;
         CREATE TABLE items (id BIGINT PRIMARY KEY, embedding vector(384) NOT NULL);",
    )
    .unwrap();
    let s = db.analyze("SELECT embedding FROM items").unwrap();
    assert_cols(
        &s,
        vec![c(
            "embedding",
            typedpg_analyzer::Type::Basic {
                schema: "public".into(),
                name: "vector".into(),
                extension: Some("vector".into()),
                typmod: Some(384),
                collation: None,
            },
        )],
    );
}

#[test]
fn vector_dimension_mismatch_in_insert_rejected() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE EXTENSION vector;
         CREATE TABLE items (id BIGINT PRIMARY KEY, embedding vector(4) NOT NULL);",
    )
    .unwrap();
    // 3 elements provided for vector(4) → PG: `expected 4 dimensions, not 3`.
    assert_analyze_err!(
        db.analyze("INSERT INTO items (id, embedding) VALUES ($p1, '[1,2,3]'::vector)"),
        AnalyzeError::Invalid(_),
        "expected 4 dimensions, not 3",
    );
}

// ── CASE / COALESCE unification ───────────────────────────────────────────

#[test]
fn cast_keeps_typmod_when_target_pinned() {
    let db = PgCatalog::new().unwrap();
    let s = db.analyze("SELECT 'hi'::varchar(10) AS s").unwrap();
    assert_cols(&s, vec![c("s", varchar_n(10))]);
}

#[test]
fn cast_strips_typmod_when_target_has_none() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (s VARCHAR(20) NOT NULL);")
        .unwrap();
    // Cast varchar(20) → text drops typmod since the target type changes.
    let s = db.analyze("SELECT s::text AS s FROM t").unwrap();
    assert_cols(&s, vec![c("s", text())]);
}

// ── Domain typmod inheritance ─────────────────────────────────────────────

#[test]
fn domain_inherits_typmod_to_column() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN short_name AS VARCHAR(20);
         CREATE TABLE t (id BIGINT PRIMARY KEY, n short_name NOT NULL);",
    )
    .unwrap();
    let s = db.analyze("SELECT n FROM t").unwrap();
    // The column inherits typmod 24 (=20+4) from the domain's base.
    assert_cols(
        &s,
        vec![c(
            "n",
            typedpg_analyzer::Type::Domain {
                schema: "public".into(),
                name: "short_name".into(),
                base: Box::new(varchar()),
                extension: None,
                typmod: Some(24),
                collation: None,
            },
        )],
    );
}

// ── UNION propagation ─────────────────────────────────────────────────────

#[test]
fn union_with_uniform_typmod_propagates() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE a (s VARCHAR(20) NOT NULL);
         CREATE TABLE b (s VARCHAR(20) NOT NULL);",
    )
    .unwrap();
    let s = db
        .analyze("SELECT s FROM a UNION ALL SELECT s FROM b")
        .unwrap();
    assert_cols(&s, vec![c("s", varchar_n(20))]);
}

#[test]
fn union_with_mixed_typmod_drops_to_none() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE a (s VARCHAR(20) NOT NULL);
         CREATE TABLE b (s VARCHAR(50) NOT NULL);",
    )
    .unwrap();
    // Different typmods on the two arms → result has no typmod.
    let s = db
        .analyze("SELECT s FROM a UNION ALL SELECT s FROM b")
        .unwrap();
    assert_cols(&s, vec![c("s", varchar())]);
}

// ── ALTER TABLE ALTER COLUMN TYPE ─────────────────────────────────────────

#[test]
fn alter_column_type_updates_typmod() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (s VARCHAR(20) NOT NULL);
         ALTER TABLE t ALTER COLUMN s TYPE VARCHAR(80);",
    )
    .unwrap();
    let s = db.analyze("SELECT s FROM t").unwrap();
    assert_cols(&s, vec![c("s", varchar_n(80))]);
}

#[test]
fn alter_column_type_to_unmodified_clears_typmod() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (s VARCHAR(20) NOT NULL);
         ALTER TABLE t ALTER COLUMN s TYPE TEXT;",
    )
    .unwrap();
    let s = db.analyze("SELECT s FROM t").unwrap();
    assert_cols(&s, vec![c("s", text())]);
}

#[test]
fn alter_column_type_to_vector_with_dim() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE EXTENSION vector;
         CREATE TABLE items (id BIGINT PRIMARY KEY, embedding vector(64) NOT NULL);
         ALTER TABLE items ALTER COLUMN embedding TYPE vector(768);",
    )
    .unwrap();
    let s = db.analyze("SELECT embedding FROM items").unwrap();
    assert_cols(
        &s,
        vec![c(
            "embedding",
            typedpg_analyzer::Type::Basic {
                schema: "public".into(),
                name: "vector".into(),
                extension: Some("vector".into()),
                typmod: Some(768),
                collation: None,
            },
        )],
    );
}

// ── UPDATE-side validation ────────────────────────────────────────────────

#[test]
fn update_varchar_too_long_rejected() {
    // Compile-time guard — PG only catches the overflow at runtime, so
    // pglite's `prepare` accepts. Opt out of the mirror.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (slug VARCHAR(3) NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("UPDATE t SET slug = 'toolong'"),
        AnalyzeError::Invalid(_),
        "value too long for type character varying(3)",
    );
}

#[test]
fn char_length_ignores_excess_spaces() {
    // varchar_input / bpchar_input drop the characters past the length
    // when they are all spaces: PG stores 'abc' for 'abc   '.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (slug VARCHAR(3) NOT NULL, code CHAR(2));")
        .unwrap();
    db.analyze("INSERT INTO t VALUES ('abc   ', 'xy  ')")
        .unwrap();
    db.analyze("UPDATE t SET slug = 'abc ', code = 'x '")
        .unwrap();
    assert_analyze_err!(
        db.analyze("UPDATE t SET slug = 'ab c'"),
        AnalyzeError::Invalid(_),
        "value too long for type character varying(3)",
    );
    assert_analyze_err!(
        db.analyze("INSERT INTO t VALUES ('abc', 'xy z')"),
        AnalyzeError::Invalid(_),
        "value too long for type character(2)",
    );
}

#[test]
fn update_numeric_overflow_rejected() {
    // Compile-time guard: PG only catches numeric overflow at execution
    // time, so pglite's `prepare` doesn't see it. Opt out of the mirror.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, amount NUMERIC(4,2) NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("UPDATE t SET amount = 12345.67 WHERE id = $p1"),
        AnalyzeError::Invalid(_),
        "numeric field overflow: a field with precision 4, scale 2 must round to an absolute value less than 10^2",
    );
}

// ── Encoder rejects invalid args ──────────────────────────────────────────

#[test]
fn varchar_with_invalid_zero_length_rejected_at_ddl() {
    let mut db = PgCatalog::new().unwrap();
    let res = db.apply_sql("CREATE TABLE t (s VARCHAR(0));");
    assert_ddl_err!(
        res,
        DdlError::UnsupportedDdl(_),
        "length for type varchar must be at least 1 (got 0)"
    );
}

#[test]
fn numeric_precision_out_of_range_rejected_at_ddl() {
    let mut db = PgCatalog::new().unwrap();
    let res = db.apply_sql("CREATE TABLE t (a NUMERIC(2000, 2));");
    assert_ddl_err!(
        res,
        DdlError::UnsupportedDdl(_),
        "NUMERIC precision 2000 must be between 1 and 1000"
    );
}

// ── typmodin ports: interval / bit / time family / validation ──────────────

#[test]
fn interval_typmods_pack_range_and_precision() {
    // intervaltypmodin packs INTERVAL_TYPMOD(precision, range); verified
    // against pg_attribute.atttypmod on PG 18.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE iv (
            c interval day NOT NULL,
            d interval(3) NOT NULL,
            e interval hour to minute NOT NULL,
            f interval month NOT NULL,
            g interval year NOT NULL,
            h interval second(2) NOT NULL,
            i interval NOT NULL
         );",
    )
    .unwrap();
    let s = db.analyze("SELECT c, d, e, f, g, h, i FROM iv").unwrap();
    let iv = |m| basic_with_typmod("pg_catalog", "interval", m);
    assert_cols(
        &s,
        vec![
            c("c", iv(589823)),
            c("d", iv(2147418115)),
            c("e", iv(201392127)),
            c("f", iv(196607)),
            c("g", iv(327679)),
            c("h", iv(268435458)),
            c("i", interval()),
        ],
    );
    let s = db
        .analyze(
            "SELECT interval '1' day AS a, '1 day'::interval(3) AS b, \
             interval '1' second(2) AS c, interval '1:2' hour to minute AS d, \
             CAST('1' AS interval hour) AS e, '1'::interval(7) AS f",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", iv(589823)),
            c("b", iv(2147418115)),
            c("c", iv(268435458)),
            c("d", iv(201392127)),
            c("e", iv(67174399)),
            // Above the maximum PG only warns and clamps to 6.
            c("f", iv(2147418118)),
        ],
    );
}

#[test]
fn bit_typmod_is_the_bare_length() {
    // anybit_typmodin stores n itself (no VARHDRSZ); plain BIT is bit(1).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE m (bb bit(8) NOT NULL, vb bit varying(4) NOT NULL);")
        .unwrap();
    let s = db.analyze("SELECT bb, vb FROM m").unwrap();
    assert_cols(
        &s,
        vec![
            c("bb", basic_with_typmod("pg_catalog", "bit", 8)),
            c("vb", basic_with_typmod("pg_catalog", "varbit", 4)),
        ],
    );
    let s = db
        .analyze("SELECT '1'::bit AS a, '1'::varbit(3) AS b, bit varying(4) '101' AS c")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", basic_with_typmod("pg_catalog", "bit", 1)),
            c("b", basic_with_typmod("pg_catalog", "varbit", 3)),
            c("c", basic_with_typmod("pg_catalog", "varbit", 4)),
        ],
    );
}

#[test]
fn array_column_typmod_comes_from_the_element_typmodin() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (vca varchar(5)[] NOT NULL, na numeric(4,2)[] NOT NULL);")
        .unwrap();
    let s = db
        .analyze("SELECT vca, na, vca[1] AS e, vca[1:2] AS f FROM t")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("vca", array_of(varchar_n(5))),
            c("na", array_of(numeric_ps(4, 2))),
            // Subscripts keep the array's typmod.
            cn("e", varchar_n(5)),
            c("f", array_of(varchar_n(5))),
        ],
    );
}

#[test]
fn time_precision_above_six_is_clamped() {
    // PG warns `TIMESTAMP(7) precision reduced to maximum allowed, 6` and
    // accepts; the value functions go through the same check.
    let db = PgCatalog::new().unwrap();
    let s = db
        .analyze(
            "SELECT '2020-01-01'::timestamp(7) AS a, '10:00'::time(7) AS b, \
             CURRENT_TIMESTAMP(7) AS c, LOCALTIME(10) AS d",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", basic_with_typmod("pg_catalog", "timestamp", 6)),
            c("b", basic_with_typmod("pg_catalog", "time", 6)),
            c("c", basic_with_typmod("pg_catalog", "timestamptz", 6)),
            c("d", basic_with_typmod("pg_catalog", "time", 6)),
        ],
    );
}

#[test]
fn invalid_typmods_rejected_like_typmodin() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE DOMAIN d AS int;").unwrap();
    for (sql, msg) in [
        (
            "SELECT 'x'::text(3)",
            "type modifier is not allowed for type \"text\"",
        ),
        (
            "SELECT 1::d(3)",
            "type modifier is not allowed for type \"d\"",
        ),
        (
            "SELECT '1'::\"char\"(3)",
            "type modifier is not allowed for type \"char\"",
        ),
        (
            "SELECT '1'::varchar(10485761)",
            "length for type varchar cannot exceed 10485760",
        ),
        (
            "SELECT '1'::char(0)",
            "length for type char must be at least 1",
        ),
        (
            "SELECT '1'::bit(83886081)",
            "length for type bit cannot exceed 83886080",
        ),
        ("SELECT '1'::\"varchar\"(3, 4)", "invalid type modifier"),
        (
            "SELECT 1::numeric(1, 2, 3)",
            "invalid NUMERIC type modifier",
        ),
        (
            "SELECT 1::numeric(2, 1001)",
            "NUMERIC scale 1001 must be between -1000 and 1000",
        ),
        (
            "SELECT '1'::\"interval\"(99)",
            "invalid INTERVAL type modifier",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::Invalid(_), msg);
    }
    // A scale above the precision is a valid typmod since PG 15 (holding
    // values below 10^-3).
    let s = db.analyze("SELECT 0.0001::numeric(2, 5) AS n").unwrap();
    assert_cols(&s, vec![c("n", numeric_ps(2, 5))]);
    assert_ddl_err!(
        try_apply(&[("001.sql", "CREATE TABLE tt (a text(3));")]),
        DdlError::UnsupportedDdl(_),
        "type modifier is not allowed for type \"text\""
    );
}

/// `typenameTypeMod` hands each modifier to `typmodin` as a string — an
/// integer, a numeric or string literal as written, a bare identifier —
/// and `ArrayGetIntegerTypmods` parses those with `pg_strtoint32`; a type
/// without a `typmodin` is named as written.
#[test]
fn typmods_are_strings_parsed_like_integers() {
    let db = PgCatalog::new().unwrap();
    for sql in [
        "SELECT '10'::bit('3') AS b",
        "SELECT '10'::bit(' 3 ') AS b",
        "SELECT '10'::bit('0x3') AS b",
        "SELECT '10'::bit(0x3) AS b",
    ] {
        let s = db.analyze(sql).unwrap();
        assert_cols(&s, vec![c("b", basic_with_typmod("pg_catalog", "bit", 3))]);
    }
    let s = db.analyze("SELECT '10'::bit('1_0') AS b").unwrap();
    assert_cols(&s, vec![c("b", basic_with_typmod("pg_catalog", "bit", 10))]);
    for (sql, msg) in [
        (
            "SELECT '10'::bit(3.14)",
            "invalid input syntax for type integer: \"3.14\"",
        ),
        (
            "SELECT '10'::bit(x)",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "SELECT 1::numeric(5, 2.0)",
            "invalid input syntax for type integer: \"2.0\"",
        ),
        (
            "SELECT '10'::bit(99999999999)",
            "value \"99999999999\" is out of range for type integer",
        ),
        (
            "SELECT '10'::bit('99999999999')",
            "value \"99999999999\" is out of range for type integer",
        ),
        (
            "SELECT '10'::bit(null)",
            "type modifiers must be simple constants or identifiers",
        ),
        (
            "SELECT '10'::bit(true)",
            "type modifiers must be simple constants or identifiers",
        ),
        (
            "SELECT '10'::bit(1 + 1)",
            "type modifiers must be simple constants or identifiers",
        ),
        (
            "SELECT 'x'::text(3.5)",
            "type modifier is not allowed for type \"text\"",
        ),
        (
            "SELECT 'x'::pg_catalog.text(3)",
            "type modifier is not allowed for type \"pg_catalog.text\"",
        ),
        (
            "SELECT 'x'::pg_catalog.text(3)[]",
            "type modifier is not allowed for type \"pg_catalog.text[]\"",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::Invalid(_), msg);
    }
}

/// A base type whose `typmodin` isn't modeled takes any simple constant or
/// identifier as a modifier (PostGIS' `geometry(Point, 4326)`); its typmod
/// isn't tracked.
#[test]
fn unmodeled_typmodin_takes_identifiers() {
    let mut db = PgCatalog::new().unwrap();
    // The stand-in `typmodin` (`bittypmodin`) takes integers only; a real
    // extension's would take the identifier PG rejects here.
    db.skip_pg_sanity();
    db.apply_sql(
        "CREATE TYPE g;
         CREATE FUNCTION g_in(cstring) RETURNS g LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
         CREATE FUNCTION g_out(g) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
         CREATE FUNCTION g_tin(cstring[]) RETURNS int LANGUAGE internal IMMUTABLE STRICT
             AS 'bittypmodin';
         CREATE TYPE g (INPUT = g_in, OUTPUT = g_out, TYPMOD_IN = g_tin,
             INTERNALLENGTH = 4, PASSEDBYVALUE, ALIGNMENT = int4);
         CREATE TABLE gt (x g(Point, 4326));",
    )
    .unwrap();
    let s = db.analyze("SELECT x FROM gt").unwrap();
    assert_cols(&s, vec![cn("x", basic("public", "g"))]);
}
