//! Parse-time validation of untyped string-literal *contents* against the
//! type a context coerces them to — mirroring PG's behavior of running the
//! target's input function on `unknown` constants during parse analysis
//! (`src/literal_input.rs`). Every rejection message must match PG verbatim
//! (the pg_sanity mirror enforces the prefix), and every acceptance must
//! agree with PG on the result type.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE status AS ENUM ('draft', 'published');
         CREATE DOMAIN posint AS INT;
         CREATE TABLE t (
            id    BIGINT PRIMARY KEY,
            n     INT,
            f     FLOAT8,
            b     BOOL NOT NULL,
            s     TEXT,
            st    status,
            pn    posint,
            nums  INT[] NOT NULL
         );",
    )
    .unwrap();
    db
}

/// `assert_analyze_err!` compares the fully rendered diagnostic; these tests
/// only care about the PG-verbatim first line, so check via starts_with.
macro_rules! assert_first_line {
    ($result:expr, $expected:expr) => {{
        let err = $result.expect_err("expected analyze to fail");
        let msg = err.to_string();
        assert!(
            msg.starts_with($expected),
            "expected message starting with {:?}, got {:?}",
            $expected,
            msg
        );
    }};
}

// ── Explicit casts ──────────────────────────────────────────────────────────

#[test]
fn cast_garbage_to_bigint_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT 'x'::bigint"),
        "invalid input syntax for type bigint: \"x\""
    );
}

#[test]
fn cast_radix_and_underscore_int_forms_accepted() {
    // PG 16+ integer input: hex/octal/binary radix prefixes and single
    // underscores between digits.
    let db = setup();
    for q in [
        "SELECT '0x1F'::int AS v",
        "SELECT '0o17'::int AS v",
        "SELECT '0b101'::int AS v",
        "SELECT '1_000'::int AS v",
        "SELECT ' +42 '::int AS v",
    ] {
        let s = db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_cols(&s, vec![c("v", int4())]);
    }
}

#[test]
fn cast_malformed_underscore_int_forms_rejected() {
    let db = setup();
    for (q, lit) in [
        ("SELECT '1__0'::int", "1__0"),
        ("SELECT '1_'::int", "1_"),
        ("SELECT '_1'::int", "_1"),
        ("SELECT '0x'::int", "0x"),
        ("SELECT '- 42'::int", "- 42"),
    ] {
        assert_first_line!(
            db.analyze(q),
            &format!("invalid input syntax for type integer: \"{lit}\"")
        );
    }
}

/// After a `0x` / `0o` / `0b` prefix an underscore may come first, as long
/// as a digit follows it (PG 16+; `int4 '0b_10_0101'` is 37 in
/// regress/sql/int4.sql) — unlike a decimal run, which can't start with one.
#[test]
fn integer_literals_allow_an_underscore_after_the_base_prefix() {
    let db = setup();
    for sql in [
        "SELECT int2 '0b_10_0101'",
        "SELECT int4 '0b_10_0101'",
        "SELECT int8 '0x_FF'",
        "SELECT '0o_17'::numeric",
        "SELECT ' 0X_30b1_F33a_6DF0 '::numeric",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (sql, ty, lit) in [
        ("SELECT '0x_'::int", "integer", "0x_"),
        ("SELECT '0b_'::int2", "smallint", "0b_"),
        ("SELECT '0x__F'::int8", "bigint", "0x__F"),
        ("SELECT '0o__7'::numeric", "numeric", "0o__7"),
    ] {
        assert_first_line!(
            db.analyze(sql),
            &format!("invalid input syntax for type {ty}: \"{lit}\"")
        );
    }
}

#[test]
fn cast_out_of_range_int_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '2147483648'::int"),
        "value \"2147483648\" is out of range for type integer"
    );
    assert_first_line!(
        db.analyze("SELECT '99999'::int2"),
        "value \"99999\" is out of range for type smallint"
    );
}

#[test]
fn cast_numeric_specials_accepted() {
    let db = setup();
    for q in [
        "SELECT 'NaN'::numeric AS v",
        "SELECT ' inf '::numeric AS v",
        "SELECT '-Infinity'::numeric AS v",
        "SELECT '1_000.5_0'::numeric AS v",
        "SELECT '0x1F'::numeric AS v",
        "SELECT '.5'::numeric AS v",
    ] {
        let s = db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_cols(&s, vec![c("v", numeric())]);
    }
}

#[test]
fn cast_malformed_numeric_rejected() {
    let db = setup();
    for (q, lit) in [
        ("SELECT '1e'::numeric", "1e"),
        ("SELECT '1.2.3'::numeric", "1.2.3"),
        ("SELECT ''::numeric", ""),
        // numeric_in: NaN mustn't have a sign (an infinity may).
        ("SELECT '+NaN'::numeric", "+NaN"),
        ("SELECT '-NaN'::numeric", "-NaN"),
    ] {
        assert_first_line!(
            db.analyze(q),
            &format!("invalid input syntax for type numeric: \"{lit}\"")
        );
    }
    db.analyze("SELECT ' NaN '::numeric AS v, '+inf'::numeric AS w")
        .unwrap();
}

#[test]
fn cast_float_specials_accepted_and_underscores_rejected() {
    let db = setup();
    // strtod accepts inf/nan and C99 hex floats…
    for q in [
        "SELECT 'inf'::float8 AS v",
        "SELECT 'NaN'::float8 AS v",
        "SELECT '0x1p3'::float8 AS v",
    ] {
        let s = db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_cols(&s, vec![c("v", float8())]);
    }
    // …but, unlike the integer family, no underscore separators.
    assert_first_line!(
        db.analyze("SELECT '1_000'::float8"),
        "invalid input syntax for type double precision: \"1_000\""
    );
}

#[test]
fn cast_bool_prefixes_accepted_ambiguous_rejected() {
    let db = setup();
    for q in [
        "SELECT 'tr'::bool AS v",
        "SELECT 'ye'::bool AS v",
        "SELECT 'of'::bool AS v",
        "SELECT ' TRUE '::bool AS v",
    ] {
        let s = db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_cols(&s, vec![c("v", bool_ty())]);
    }
    // `o` is an ambiguous prefix of on/off; `10` is not a bool.
    assert_first_line!(
        db.analyze("SELECT 'o'::bool"),
        "invalid input syntax for type boolean: \"o\""
    );
    assert_first_line!(
        db.analyze("SELECT '10'::bool"),
        "invalid input syntax for type boolean: \"10\""
    );
}

#[test]
fn cast_uuid_variants() {
    let db = setup();
    // Braced and unhyphenated forms are valid; whitespace and misplaced
    // hyphens are not.
    for q in [
        "SELECT '{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}'::uuid AS v",
        "SELECT 'a0eebc999c0b4ef8bb6d6bb9bd380a11'::uuid AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    assert_first_line!(
        db.analyze("SELECT ' a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11 '::uuid"),
        "invalid input syntax for type uuid: \" a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11 \""
    );
}

#[test]
fn cast_json_structural_validation() {
    let db = setup();
    for q in [
        r#"SELECT '{"a": [1, -0.5e3, true, null]}'::jsonb AS v"#,
        "SELECT '1.5e3'::json AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    // PG's json message carries no content (details go in DETAIL).
    for q in [
        "SELECT '01'::json",
        "SELECT '[1,]'::jsonb",
        "SELECT 'nullx'::json",
        "SELECT ''::jsonb",
    ] {
        assert_first_line!(db.analyze(q), "invalid input syntax for type json");
    }
}

#[test]
fn cast_malformed_array_and_range_literals_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT 'oops'::int[]"),
        "malformed array literal: \"oops\""
    );
    assert_first_line!(
        db.analyze("SELECT ''::int4range"),
        "malformed range literal: \"\""
    );
    // `empty` (any case, padded) and bracketed forms pass the structural
    // check; `{…}` arrays and `[1:2]={…}` dimension forms too.
    for q in [
        "SELECT ' EMPTY '::int4range AS v",
        "SELECT '(1,2]'::int4range AS v",
        "SELECT '{1,2}'::int[] AS v",
        "SELECT '[1:2]={1,2}'::int[] AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

#[test]
fn cast_invalid_enum_label_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT 'bogus'::status"),
        "invalid input value for enum status: \"bogus\""
    );
    db.analyze("SELECT 'draft'::status AS v").unwrap();
}

#[test]
fn cast_domain_validates_base_type_content() {
    // Domain values go through the *base* type's input function, and PG's
    // message names the base type.
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT 'x'::posint"),
        "invalid input syntax for type integer: \"x\""
    );
}

#[test]
fn cast_empty_string_to_datetime_family_rejected() {
    let db = setup();
    // The input functions are too complex to model, but '' is known-invalid.
    // Note the input functions' own type names: `timestamp`, not
    // `timestamp without time zone`.
    assert_first_line!(
        db.analyze("SELECT ''::timestamp"),
        "invalid input syntax for type timestamp: \"\""
    );
    assert_first_line!(
        db.analyze("SELECT ''::timestamptz"),
        "invalid input syntax for type timestamp with time zone: \"\""
    );
    assert_first_line!(
        db.analyze("SELECT ''::point"),
        "invalid input syntax for type point: \"\""
    );
    // Non-empty contents are accepted unchecked (conservative).
    db.analyze("SELECT 'now'::date AS v").unwrap();
}

#[test]
fn cast_regclass_and_regproc_resolved_against_catalog() {
    let db = setup();
    db.analyze("SELECT 't'::regclass AS v").unwrap();
    db.analyze("SELECT '123'::regclass AS v").unwrap();
    db.analyze("SELECT 'now'::regproc AS v").unwrap();
    // `regproc` needs a *unique* name — `length` has several overloads.
    assert_first_line!(
        db.analyze("SELECT 'length'::regproc"),
        "more than one function named \"length\""
    );
    assert_first_line!(
        db.analyze("SELECT 'no_such_table'::regclass"),
        "relation \"no_such_table\" does not exist"
    );
    assert_first_line!(
        db.analyze("SELECT 'no_such_fn'::regproc"),
        "function \"no_such_fn\" does not exist"
    );
    // Unquoted names with embedded whitespace fail identifier splitting.
    assert_first_line!(
        db.analyze("SELECT '1 day'::regclass"),
        "invalid name syntax"
    );
    assert_first_line!(db.analyze("SELECT ''::regproc"), "invalid name syntax");
    // A failed function / operator lookup names the input as written, not
    // the identifier it split into (downcased, trimmed).
    assert_first_line!(
        db.analyze("SELECT ' EMPTY '::regproc"),
        "function \" EMPTY \" does not exist"
    );
    assert_first_line!(
        db.analyze("SELECT 'NaN'::regproc"),
        "function \"NaN\" does not exist"
    );
    assert_first_line!(
        db.analyze("SELECT ' 42 '::regproc"),
        "function \" 42 \" does not exist"
    );
    assert_first_line!(
        db.analyze("SELECT 'No_Such_Op'::regoper"),
        "operator does not exist: No_Such_Op"
    );
}

// ── Coercion contexts beyond the explicit cast ──────────────────────────────

#[test]
fn operator_coercion_validates_literal() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT id FROM t WHERE n > 'hello'"),
        "invalid input syntax for type integer: \"hello\""
    );
    // Valid content flows through the same path.
    db.analyze("SELECT id FROM t WHERE n > '41'").unwrap();
}

#[test]
fn where_clause_validates_bare_literal() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT 1 WHERE 'x'"),
        "invalid input syntax for type boolean: \"x\""
    );
}

#[test]
fn limit_validates_literal() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT 1 LIMIT 'x'"),
        "invalid input syntax for type bigint: \"x\""
    );
}

#[test]
fn insert_assignment_validates_literal() {
    let db = setup();
    assert_first_line!(
        db.analyze("INSERT INTO t (n) VALUES ('x')"),
        "invalid input syntax for type integer: \"x\""
    );
}

#[test]
fn greatest_backfill_validates_literal() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT GREATEST(1, 'x')"),
        "invalid input syntax for type integer: \"x\""
    );
    let s = db.analyze("SELECT GREATEST(1, '5') AS v").unwrap();
    assert_cols(&s, vec![c("v", int4())]);
}

#[test]
fn function_arg_backfill_validates_literal() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT sqrt('x')"),
        "invalid input syntax for type double precision: \"x\""
    );
}

// ── Special-form comparison fixes (operator resolution, not coercion) ───────

#[test]
fn between_mixed_numeric_bounds_accepted() {
    // `x >= lo AND x <= hi` resolves each comparison independently —
    // int4 <= numeric exists, so this is valid despite numeric ⊄ int4.
    let db = setup();
    let s = db
        .analyze("SELECT id FROM t WHERE n BETWEEN 18 AND 3.14")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn between_incomparable_bound_rejected_with_operator_error() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT id FROM t WHERE n BETWEEN 1 AND '{}'::jsonb"),
        "operator does not exist: integer <= jsonb"
    );
}

#[test]
fn between_unknown_bound_content_validated() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT id FROM t WHERE n BETWEEN 'x' AND 10"),
        "invalid input syntax for type integer: \"x\""
    );
}

#[test]
fn in_list_mixed_numeric_items_accepted() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM t WHERE n IN (18, 3.14)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn in_list_unknown_item_content_validated() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT id FROM t WHERE n IN (1, 'x')"),
        "invalid input syntax for type integer: \"x\""
    );
}

#[test]
fn is_distinct_from_comparable_types_accepted() {
    let db = setup();
    let s = db
        .analyze("SELECT n IS DISTINCT FROM 3.14 AS v FROM t")
        .unwrap();
    assert_cols(&s, vec![c("v", bool_ty())]);
}

#[test]
fn is_distinct_from_incomparable_types_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT s IS DISTINCT FROM 5 FROM t"),
        "operator does not exist: text = integer"
    );
}

// ── Clause-specific message rewrites ────────────────────────────────────────

#[test]
fn array_subscript_requires_integer() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT nums[s] FROM t"),
        "array subscript must have type integer"
    );
}

#[test]
fn filter_clause_requires_boolean() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT count(*) FILTER (WHERE now()) FROM t"),
        "argument of FILTER must be type boolean, not type timestamp with time zone"
    );
}

#[test]
fn greatest_unmatched_types_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT GREATEST(s, 5) FROM t"),
        "GREATEST types text and integer cannot be matched"
    );
    assert_first_line!(
        db.analyze("SELECT LEAST(b, 1) FROM t"),
        "LEAST types boolean and integer cannot be matched"
    );
}

#[test]
fn oid_range_checked() {
    let db = setup();
    // strtoul wrap-around semantics: positive values must fit uint32;
    // negative magnitudes must fit int32 (`'-1'` is 4294967295).
    db.analyze("SELECT '-1'::oid AS v").unwrap();
    assert_first_line!(
        db.analyze("SELECT '99999999999999999999'::oid"),
        "value \"99999999999999999999\" is out of range for type oid"
    );
    assert_first_line!(
        db.analyze("SELECT '-4294967295'::oid"),
        "value \"-4294967295\" is out of range for type oid"
    );
    // The reg* OID-literal path shares the range check.
    assert_first_line!(
        db.analyze("SELECT '9999999999999999999999'::regproc"),
        "value \"9999999999999999999999\" is out of range for type oid"
    );
}

#[test]
fn array_dimension_form_requires_separator() {
    let db = setup();
    // `[…]` openers are only valid as the explicit-dimensions form
    // `[lo:hi]={…}` — a bare bracket list is malformed.
    assert_first_line!(
        db.analyze("SELECT '[1,]'::int4[]"),
        "malformed array literal: \"[1,]\""
    );
    db.analyze("SELECT '[1:2]={1,2}'::int4[] AS v").unwrap();
}

#[test]
fn regtype_bare_identifier_resolved_against_catalog() {
    let db = setup();
    db.analyze("SELECT 'integer'::regtype AS v").unwrap();
    db.analyze("SELECT 'status'::regtype AS v").unwrap();
    assert_first_line!(
        db.analyze("SELECT 'NaN'::regtype"),
        "type \"nan\" does not exist"
    );
    // An empty or blank value fails before the type-name grammar runs.
    for blank in ["", "   "] {
        assert_first_line!(
            db.analyze(&format!("SELECT '{blank}'::regtype")),
            &format!("invalid type name \"{blank}\"")
        );
    }
    // Anything beyond a bare identifier uses the full type grammar — skip.
    db.analyze("SELECT 'character varying'::regtype AS v")
        .unwrap();
}

#[test]
fn any_all_with_concrete_incompatible_sides_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT id FROM t WHERE s = ANY(ARRAY[1, 2, 3])"),
        "operator does not exist: text = integer"
    );
    db.analyze("SELECT id FROM t WHERE n = ANY(ARRAY[1, 2, 3])")
        .unwrap();
    // Cross-type comparisons still resolve through the operator catalog.
    db.analyze("SELECT id FROM t WHERE id = ANY(ARRAY[1, 2, 3])")
        .unwrap();
}

#[test]
fn datetime_keywords_accepted() {
    let db = setup();
    for q in [
        "SELECT 'now'::date AS v",
        "SELECT ' Today '::timestamptz AS v",
        "SELECT 'epoch'::timestamp AS v",
        "SELECT '+infinity'::date AS v",
        "SELECT '-Infinity'::timestamp AS v",
        "SELECT 'allballs'::time AS v",
        "SELECT 'now'::timetz AS v",
        "SELECT 'infinity'::interval AS v",
        // Multi-field values are decoded like PG's datetime parser.
        "SELECT '2024-01-01'::date AS v",
        "SELECT '1 day'::interval AS v",
        "SELECT 'now()'::date AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

#[test]
fn datetime_bare_words_rejected() {
    // Purely alphabetic tokens that aren't a special keyword are always
    // `invalid input syntax` in PG's datetime lexer.
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT 'hello'::timestamptz"),
        "invalid input syntax for type timestamp with time zone: \"hello\""
    );
    assert_first_line!(
        db.analyze("SELECT 'jan'::date"),
        "invalid input syntax for type date: \"jan\""
    );
    // Keywords don't cross type families: time has no epoch/infinity, and
    // interval has no today.
    assert_first_line!(
        db.analyze("SELECT 'epoch'::time"),
        "invalid input syntax for type time: \"epoch\""
    );
    assert_first_line!(
        db.analyze("SELECT 'infinity'::time"),
        "invalid input syntax for type time: \"infinity\""
    );
    assert_first_line!(
        db.analyze("SELECT 'today'::interval"),
        "invalid input syntax for type interval: \"today\""
    );
    assert_first_line!(
        db.analyze("SELECT 'allballs'::timestamp"),
        "invalid input syntax for type timestamp: \"allballs\""
    );
}

#[test]
fn any_all_requires_array_on_right_side() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT id FROM t WHERE n = ANY(42)"),
        "op ANY/ALL (array) requires array on right side"
    );
    assert_first_line!(
        db.analyze("SELECT id FROM t WHERE n = ALL(b)"),
        "op ANY/ALL (array) requires array on right side"
    );
    // An UNKNOWN right side is fine — it's coerced to the element array.
    db.analyze("SELECT id FROM t WHERE n = ANY('{1,2}')")
        .unwrap();
    db.analyze("SELECT id FROM t WHERE n = ANY(nums)").unwrap();
}

#[test]
fn cast_malformed_multirange_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT ''::int4multirange"),
        "malformed multirange literal: \"\""
    );
    assert_first_line!(
        db.analyze("SELECT 'x'::int4multirange"),
        "malformed multirange literal: \"x\""
    );
    for q in [
        "SELECT '{}'::int4multirange AS v",
        "SELECT ' {[1,2)} '::int4multirange AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

#[test]
fn cast_to_input_refusing_system_types_rejected() {
    // These internal types' input functions refuse any value — note the
    // brin_minmax message drops the `pg_` prefix (PG's own string).
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT 'x'::pg_node_tree"),
        "cannot accept a value of type pg_node_tree"
    );
    assert_first_line!(
        db.analyze("SELECT 'x'::pg_brin_minmax_multi_summary"),
        "cannot accept a value of type brin_minmax_multi_summary"
    );
}

#[test]
fn cast_empty_string_to_system_identifier_types_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT ''::tid"),
        "invalid input syntax for type tid: \"\""
    );
    assert_first_line!(
        db.analyze("SELECT ''::xid"),
        "invalid input syntax for type xid: \"\""
    );
    // Non-empty contents stay unchecked (conservative).
    db.analyze("SELECT '42'::xid AS v").unwrap();
}

/// tidin reads each number with strtoul, which wraps a `-` (`'(-1,0)'` is
/// block 4294967295) and must stop at the delimiter; the block must fit 32
/// bits, the offset 16. Nothing before the `(` or after the `)` is looked
/// at.
#[test]
fn tid_input_follows_tidin_bounds() {
    let db = setup();
    for sql in [
        "SELECT '(-1,0)'::tid AS v",
        "SELECT '(,1)'::tid AS v",
        "SELECT 'xyz(1,2)'::tid AS v",
        "SELECT '(1,2)garbage'::tid AS v",
        "SELECT '(4294967295,65535)'::tid AS v",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for lit in ["(4294967296,1)", "(1,65536)", "(1,-1)", "( 1 ,2)"] {
        assert_first_line!(
            db.analyze(&format!("SELECT '{lit}'::tid")),
            &format!("invalid input syntax for type tid: \"{lit}\"")
        );
    }
}

/// pg_lsn_in takes hex digits around a `/` and nothing else.
#[test]
fn pg_lsn_rejects_leading_whitespace() {
    let db = setup();
    for lit in [" 0/12345678", "0/12345678 "] {
        assert_first_line!(
            db.analyze(&format!("SELECT '{lit}'::pg_lsn")),
            &format!("invalid input syntax for type pg_lsn: \"{lit}\"")
        );
    }
    db.analyze("SELECT 'FFFFFFFF/0'::pg_lsn AS v").unwrap();
}

/// cash_in accumulates the amount in int64 cents: one cent past either end
/// (or a third decimal rounding past it) is out of range.
#[test]
fn money_rejects_out_of_range_values() {
    let db = setup();
    for v in [
        "92233720368547758.08",
        "-92233720368547758.09",
        "92233720368547758.075",
    ] {
        assert_first_line!(
            db.analyze(&format!("SELECT '{v}'::money")),
            &format!("value \"{v}\" is out of range for type money")
        );
    }
    db.analyze(
        "SELECT '92233720368547758.07'::money AS v, '-92233720368547758.08'::money AS w, \
         '92233720368547758.074'::money AS x",
    )
    .unwrap();
}

/// parse_snapshot checks xmin and xmax with FullTransactionIdIsValid, which
/// looks at the low 32 bits only: an epoch-aligned xid (2^32, 2^63) is
/// invalid however large.
#[test]
fn pg_snapshot_rejects_an_overflowing_xid() {
    let db = setup();
    for lit in [
        "1:9223372036854775808:3",
        "1:4294967296:",
        "4294967296:4294967297:",
    ] {
        assert_first_line!(
            db.analyze(&format!("SELECT txid_snapshot '{lit}'")),
            &format!("invalid input syntax for type pg_snapshot: \"{lit}\"")
        );
    }
    db.analyze(
        "SELECT pg_snapshot '1:9223372036854775809:3' AS v, \
         pg_snapshot '4294967297:4294967298:4294967297' AS w",
    )
    .unwrap();
}

#[test]
fn iso_date_field_overflow_rejected() {
    // DecodeDate reads `yyyy-mm-dd` as year, month, day and ValidateDate
    // rejects an impossible month / day (verified on PG 18).
    let db = setup();
    for sql in [
        "SELECT CAST('2020-13-01' AS date) AS a",
        "SELECT '2020-02-30'::date",
        "SELECT '2021-02-29'::date",
        "SELECT '1900-02-29'::timestamp",
        "SELECT '2020-00-10'::timestamptz",
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(
            err.to_string()
                .starts_with("date/time field value out of range: \""),
            "{sql}: {err}"
        );
    }
    for sql in [
        "SELECT '2020-02-29'::date",
        "SELECT '2000-02-29'::date",
        "SELECT '99999-12-31'::date",
    ] {
        db.analyze(sql).unwrap();
    }
}

// ── record-typed literals and polymorphic defaults ──────────────────────────

#[test]
fn anonymous_record_literal_input_rejected() {
    // record_in has no row type to parse into (PG 18: 0A000 at prepare).
    let db = setup();
    for sql in [
        "SELECT min('(1,2)'::record) AS c",
        "SELECT '(1,2)'::record AS c",
        "SELECT ROW(1, 2) = '(1,2)' AS c",
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(
            err.to_string()
                .starts_with("input of anonymous composite types is not implemented"),
            "{sql}: {err}"
        );
    }
    // An empty record[] literal never calls record_in.
    db.analyze("SELECT '{}'::record[] AS c").unwrap();
}

#[test]
fn polymorphic_function_with_defaulted_trailing_args() {
    // json_populate_record(base anyelement, from_json json,
    // use_json_as_text bool DEFAULT false): PG prepares both calls (the
    // NULL::record one only fails at execution) and types the result as
    // `base`'s type.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TYPE comp AS (a int);").unwrap();
    let s = db
        .analyze("SELECT json_populate_record(NULL::record, '{}') IS NULL AS b")
        .unwrap();
    assert_cols(&s, vec![c("b", bool_ty())]);
    let s = db
        .analyze("SELECT (json_populate_record(NULL::comp, '{}')).a AS a")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4())]);
}

// ── Datetime decoding (a port of PG's datetime.c) ──────────────────────────

#[test]
fn time_field_out_of_range_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '12:61'::time"),
        "date/time field value out of range: \"12:61\""
    );
    assert_first_line!(
        db.analyze("SELECT '25:00'::time"),
        "date/time field value out of range: \"25:00\""
    );
    // 24:00:00 and a leap second are valid; anything past midnight isn't.
    db.analyze("SELECT '24:00:00'::time AS v").unwrap();
    db.analyze("SELECT '23:59:60'::time AS v").unwrap();
    assert_first_line!(
        db.analyze("SELECT '24:00:01'::time"),
        "date/time field value out of range: \"24:00:01\""
    );
}

/// A full zone name must be in the tz database, which the analyzer lists
/// in full; only the trees some distributions add (`posix/…`, `right/…`)
/// and the legacy links are left to the server.
#[test]
fn timestamptz_rejects_an_unknown_time_zone() {
    let db = setup();
    for (lit, zone) in [
        (
            "19970710 173201 America/Does_not_exist",
            "america/does_not_exist",
        ),
        ("2020-01-01 10:00 Europe/Nowhere", "europe/nowhere"),
    ] {
        assert_first_line!(
            db.analyze(&format!("SELECT '{lit}'::timestamptz")),
            &format!("time zone \"{zone}\" not recognized")
        );
    }
    db.analyze("SELECT '2020-01-01 10:00 America/Ciudad_Juarez'::timestamptz AS v")
        .unwrap();
}

#[test]
fn interval_unknown_unit_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '1 fortnight'::interval"),
        "invalid input syntax for type interval: \"1 fortnight\""
    );
    assert_first_line!(
        db.analyze("SELECT '1 xday'::interval"),
        "invalid input syntax for type interval: \"1 xday\""
    );
}

#[test]
fn window_range_offset_interval_literal_validated() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE ev (id INT, ts TIMESTAMPTZ);")
        .unwrap();
    db.analyze("SELECT sum(id) OVER (ORDER BY ts RANGE '1 day' PRECEDING) AS s FROM ev")
        .unwrap();
    assert_first_line!(
        db.analyze("SELECT sum(id) OVER (ORDER BY ts RANGE '1 xday' PRECEDING) AS s FROM ev"),
        "invalid input syntax for type interval: \"1 xday\""
    );
}

#[test]
fn generate_series_step_interval_literal_validated() {
    let db = setup();
    db.analyze("SELECT generate_series('2020-01-01'::timestamptz, '2020-02-01', '1 day') AS g")
        .unwrap();
    assert_first_line!(
        db.analyze(
            "SELECT generate_series('2020-01-01'::timestamptz, '2020-02-01', '1 xday') AS g"
        ),
        "invalid input syntax for type interval: \"1 xday\""
    );
}

#[test]
fn timestamptz_minus_unknown_literal_validated_as_timestamptz() {
    // `timestamptz - unknown` resolves to `timestamptz - timestamptz`, so
    // the literal is timestamptz input, not an interval.
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT now() - '1 day'"),
        "invalid input syntax for type timestamp with time zone: \"1 day\""
    );
    db.analyze("SELECT now() - '2024-01-01 12:00+02' AS v")
        .unwrap();
    db.analyze("SELECT now() - '1 day'::interval AS v").unwrap();
}

#[test]
fn datetime_multi_field_values_decoded() {
    let db = setup();
    for q in [
        "SELECT '2024-02-29'::date AS v",
        "SELECT 'Jan 15, 2024 BC'::date AS v",
        "SELECT 'J2451187'::date AS v",
        "SELECT '2024-01-15T12:30:45.5Z'::timestamptz AS v",
        "SELECT '2024-01-15 12:30 America/Sao_Paulo'::timestamptz AS v",
        "SELECT '12:30 pm'::time AS v",
        "SELECT 'yesterday 10:00'::timestamp AS v",
        "SELECT 'P1Y2M3DT4H5M6S'::interval AS v",
        "SELECT '@ 1 day ago'::interval AS v",
        "SELECT '1 2:03:04'::interval AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    assert_first_line!(
        db.analyze("SELECT '2023-02-29'::date"),
        "date/time field value out of range: \"2023-02-29\""
    );
    assert_first_line!(
        db.analyze("SELECT '2024-01-15 12:00 +16'::timestamptz"),
        "time zone displacement out of range: \"2024-01-15 12:00 +16\""
    );
    assert_first_line!(
        db.analyze("SELECT '2024-01-15 12:00 Foo/Bar'::timestamptz"),
        "time zone \"foo/bar\" not recognized"
    );
    assert_first_line!(
        db.analyze("SELECT '5874898-01-01'::date"),
        "date out of range: \"5874898-01-01\""
    );
    assert_first_line!(
        db.analyze("SELECT '294277-01-01'::timestamp"),
        "timestamp out of range: \"294277-01-01\""
    );
    assert_first_line!(
        db.analyze("SELECT '2147483648 days'::interval"),
        "interval field value out of range: \"2147483648 days\""
    );
    assert_first_line!(
        db.analyze("SELECT '178956971 years'::interval"),
        "interval out of range"
    );
}
// ── array literal contents (array_in) ───────────────────────────────────────

#[test]
fn array_literal_element_validated_against_element_type() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '{a}'::int[]"),
        "invalid input syntax for type integer: \"a\""
    );
    assert_first_line!(
        db.analyze("SELECT '{1,2147483648}'::int[]"),
        "value \"2147483648\" is out of range for type integer"
    );
    // The element error wins over a structural error further right, as
    // array_in calls the element input function token by token.
    assert_first_line!(
        db.analyze("SELECT '{a,'::int[]"),
        "invalid input syntax for type integer: \"a\""
    );
    // Quoted / escaped / padded elements are de-quoted before validation.
    for q in [
        "SELECT '{\"1\", 2 ,\\3}'::int[] AS v",
        "SELECT '{1\\ }'::int[] AS v",
        "SELECT '{NULL,null}'::int[] AS v",
        "SELECT '[0:1]={1,2}'::int[] AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    // A quoted "NULL" is the string, not a NULL element.
    assert_first_line!(
        db.analyze("SELECT '{\"NULL\"}'::int[]"),
        "invalid input syntax for type integer: \"NULL\""
    );
}

#[test]
fn array_literal_structure_rejected() {
    let db = setup();
    for lit in [
        "{1,2",
        "{{1},{2,3}}",
        "{1,}",
        "{\"a\"b}",
        "[1:3]={1,2}",
        "{1} x",
    ] {
        assert_first_line!(
            db.analyze(&format!("SELECT '{lit}'::int[]")),
            &format!("malformed array literal: \"{lit}\"")
        );
    }
    assert_first_line!(
        db.analyze("SELECT '[2:1]={1}'::int[]"),
        "upper bound cannot be less than lower bound"
    );
    assert_first_line!(
        db.analyze("SELECT '{{{{{{{1}}}}}}}'::int[]"),
        "number of array dimensions exceeds the maximum allowed (6)"
    );
    assert_first_line!(
        db.analyze("SELECT '[1:99999999999]={1}'::int[]"),
        "array bound is out of integer range"
    );
}

#[test]
fn array_literal_insert_assignment_validates_elements() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id INT PRIMARY KEY, name TEXT, arr INT[]);")
        .unwrap();
    assert_first_line!(
        db.analyze("INSERT INTO t (id, name, arr) VALUES (1, 'a', '{a}')"),
        "invalid input syntax for type integer: \"a\""
    );
    db.analyze("INSERT INTO t (id, name, arr) VALUES (1, 'a', '{1,NULL}')")
        .unwrap();
}

#[test]
fn array_literal_box_elements_use_semicolon_delimiter() {
    let db = setup();
    db.analyze("SELECT '{(1,2),(3,4);(5,6),(7,8)}'::box[] AS v")
        .unwrap();
    assert_first_line!(
        db.analyze("SELECT '{(1,2),(3,4),(5,6),(7,8)}'::box[]"),
        "invalid input syntax for type box: \"(1,2),(3,4),(5,6),(7,8)\""
    );
}

#[test]
fn array_literal_null_element_of_not_null_domain_rejected() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE DOMAIN nn AS int NOT NULL; CREATE DOMAIN nn2 AS nn;")
        .unwrap();
    assert_first_line!(
        db.analyze("SELECT '{NULL}'::nn[]"),
        "domain nn does not allow null values"
    );
    assert_first_line!(
        db.analyze("SELECT '{1,NULL}'::nn2[]"),
        "domain nn2 does not allow null values"
    );
    db.analyze("SELECT cardinality('{1}'::nn2[]) AS v").unwrap();
}

// ── float range (float8in_internal / float4in_internal) ─────────────────────

#[test]
fn float_literal_overflow_rejected() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '1e400'::float8"),
        "\"1e400\" is out of range for type double precision"
    );
    assert_first_line!(
        db.analyze("SELECT '1e40'::float4"),
        "\"1e40\" is out of range for type real"
    );
    // The range error names just the number and wins over trailing junk.
    assert_first_line!(
        db.analyze("SELECT ' 1e400x'::float8"),
        "\"1e400\" is out of range for type double precision"
    );
    // Assignment coercion runs the same input function.
    assert_first_line!(
        db.analyze("UPDATE t SET f = '-1e400'"),
        "\"-1e400\" is out of range for type double precision"
    );
}

#[test]
fn float_literal_underflow_rejected_but_subnormals_accepted() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '1e-400'::float8"),
        "\"1e-400\" is out of range for type double precision"
    );
    assert_first_line!(
        db.analyze("SELECT '1e-50'::float4"),
        "\"1e-50\" is out of range for type real"
    );
    for q in [
        "SELECT '1e-310'::float8 AS v",
        "SELECT '1e-40'::float4 AS v",
        "SELECT '0e-400'::float8 AS v",
        "SELECT 'nan(1)'::float8 AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

// ── range / multirange literals (range_in, multirange_in) ───────────────────

#[test]
fn range_literal_lower_above_upper_rejected() {
    let db = setup();
    for q in [
        "SELECT '[1,0]'::int4range",
        "SELECT '[1.5,1.4]'::numrange",
        "SELECT '[NaN,1]'::numrange",
        "SELECT '[2024-01-02,2024-01-01]'::daterange",
        "SELECT '[2024-01-01 10:00,2024-01-01 09:00]'::tsrange",
        // 01:00 -08 is 09:00 UTC, after 02:00 -05 (07:00 UTC).
        "SELECT '[2010-01-01 01:00:00 -08, 2010-01-01 02:00:00 -05)'::tstzrange",
    ] {
        assert_first_line!(
            db.analyze(q),
            "range lower bound must be less than or equal to range upper bound"
        );
    }
    // Equal bounds make an empty (or single-point) range; unbounded sides
    // never conflict.
    for q in [
        "SELECT '[1,1)'::int4range AS v",
        "SELECT '[1,1]'::int4range AS v",
        "SELECT '(,)'::int4range AS v",
        "SELECT '[1.5,1.50]'::numrange AS v",
        "SELECT '[2010-01-01 02:00:00 -05, 2010-01-01 01:00:00 -08)'::tstzrange AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

/// A user-defined range orders its bounds by the subtype's default btree
/// operator class — text under its collation, computable for "C".
#[test]
fn user_defined_range_literals_check_bound_order() {
    let mut db = setup();
    db.apply_sql(
        r#"CREATE TYPE textrange AS RANGE (subtype = text, collation = "C");
           CREATE TYPE intrange AS RANGE (subtype = int4, subtype_opclass = int4_ops);"#,
    )
    .unwrap();
    for q in [
        "SELECT '[z,a]'::textrange",
        "SELECT '{[z,a]}'::textmultirange",
        "SELECT '[Z,A]'::textrange",
        "SELECT '[2,1]'::intrange",
    ] {
        assert_first_line!(
            db.analyze(q),
            "range lower bound must be less than or equal to range upper bound"
        );
    }
    for q in [
        "SELECT '[a,z]'::textrange AS v",
        // "C" orders upper case before lower case.
        "SELECT '[Z,a]'::textrange AS v",
        "SELECT '{[a,b], [c,z]}'::textmultirange AS v",
        "SELECT '[1,2]'::intrange AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

#[test]
fn range_literal_bounds_validated_with_subtype() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '[a,2)'::int4range"),
        "invalid input syntax for type integer: \"a\""
    );
    assert_first_line!(
        db.analyze("SELECT '[1,2,3)'::int4range"),
        "malformed range literal: \"[1,2,3)\""
    );
    // int4range_canonical can't shift an inclusive upper bound past MAX.
    assert_first_line!(
        db.analyze("SELECT '[1,2147483647]'::int4range"),
        "integer out of range"
    );
}

#[test]
fn multirange_literal_structure_and_members_validated() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '{[1,2)'::int4multirange"),
        "malformed multirange literal: \"{[1,2)\""
    );
    assert_first_line!(
        db.analyze("SELECT '{[1,2),}'::int4multirange"),
        "malformed multirange literal: \"{[1,2),}\""
    );
    // Member ranges report with their own text.
    assert_first_line!(
        db.analyze("SELECT '{[1,2),[4,3)}'::int4multirange"),
        "range lower bound must be less than or equal to range upper bound"
    );
    assert_first_line!(
        db.analyze("SELECT '{[1,\"2)\"]}'::int4multirange"),
        "invalid input syntax for type integer: \"2)\""
    );
    db.analyze("SELECT '{empty, [1,2), (3,4]}'::int4multirange AS v")
        .unwrap();
}

// ── bytea / tsquery / tsvector / xml / cidr ─────────────────────────────────

#[test]
fn bytea_literal_hex_and_escape_formats_validated() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '\\xZZ'::bytea"),
        "invalid hexadecimal digit: \"Z\""
    );
    assert_first_line!(
        db.analyze("SELECT E'\\\\x4'::bytea"),
        "invalid hexadecimal data: odd number of digits"
    );
    assert_first_line!(
        db.analyze("SELECT 'ab\\401c'::bytea"),
        "invalid input syntax for type bytea"
    );
    for q in [
        "SELECT '\\x 41 42'::bytea AS v",
        "SELECT 'ab\\\\c\\101'::bytea AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

#[test]
fn tsquery_literal_parsed_like_tsqueryin() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT 'a &'::tsquery"),
        "no operand in tsquery: \"a &\""
    );
    assert_first_line!(
        db.analyze("SELECT 'a b'::tsquery"),
        "syntax error in tsquery: \"a b\""
    );
    assert_first_line!(
        db.analyze("SELECT 'a <16385> b'::tsquery"),
        "distance in phrase operator must be an integer value between zero and 16384 inclusive"
    );
    // No dictionary is involved: stop words and the empty query are fine.
    for q in [
        "SELECT ''::tsquery AS v",
        "SELECT 'the & a'::tsquery AS v",
        "SELECT '(a | b) & !c <-> d:AB*'::tsquery AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

#[test]
fn tsvector_literal_parsed_like_tsvectorin() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '''a b'::tsvector"),
        "syntax error in tsvector: \"'a b\""
    );
    assert_first_line!(
        db.analyze("SELECT 'a:0'::tsvector"),
        "wrong position info in tsvector: \"a:0\""
    );
    db.analyze("SELECT 'a:1A,2b ''c d'' &'::tsvector AS v")
        .unwrap();
}

#[test]
fn xml_literal_must_be_well_formed_content() {
    let db = setup();
    for lit in ["<a>", "<a></b>", "<a x=1/>", "a & b", "<!-- a -- b -->"] {
        assert_first_line!(
            db.analyze(&format!("SELECT '{lit}'::xml")),
            "invalid XML content"
        );
    }
    assert_first_line!(
        db.analyze("SELECT '<?xml version=1.0?><a/>'::xml"),
        "invalid XML content: invalid XML declaration"
    );
    for q in [
        "SELECT 'plain text'::xml AS v",
        "SELECT '<a x=\"1\">t<b/><!-- c --></a><c/>'::xml AS v",
        "SELECT '<?xml version=\"1.0\"?><a>&amp;&#65;</a>'::xml AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

/// A DOCTYPE makes xml_in parse the value as a document: one root element,
/// with only comments, PIs and whitespace around it.
#[test]
fn xml_content_with_a_doctype_must_be_a_document() {
    let db = setup();
    for lit in [
        "<!DOCTYPE a><a/><b/>",
        "<!DOCTYPE a>",
        "<!DOCTYPE a>text<a/>",
        "<!DOCTYPE a><a/>text",
        "<!DOCTYPE a><a>&foo;</a>",
    ] {
        assert_first_line!(
            db.analyze(&format!("SELECT xml '{lit}'")),
            "invalid XML content"
        );
    }
    for q in [
        "SELECT xml '<!DOCTYPE a><a/>' AS v",
        "SELECT xml '<!--x--><!DOCTYPE a ><b>t<c/></b> <!--y-->' AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

/// jsonpath's scanner pairs UTF-16 surrogates within one run of `\u`
/// escapes (addUnicode); an unpaired half is an error.
#[test]
fn jsonpath_rejects_unpaired_surrogates() {
    let db = setup();
    for lit in [
        r#""\ud83d\ud83d""#,
        r#""\ude04\ud83d""#,
        r#""\ud83dX""#,
        r#"$."\ude04X""#,
        r#""\ud83d\x41""#,
    ] {
        assert_first_line!(
            db.analyze(&format!("SELECT '{lit}'::jsonpath")),
            "invalid input syntax for type jsonpath"
        );
    }
    // A bad escape ending a run is reported with the whole run.
    assert_first_line!(
        db.analyze(r#"SELECT '"\ud83d\u12"'::jsonpath"#),
        r#"invalid Unicode escape sequence at or near "\ud83d\u12" of jsonpath input"#
    );
    db.analyze(r#"SELECT '"\ud83d\ude04"'::jsonpath AS v, '$."\u{1F604}"'::jsonpath AS w"#)
        .unwrap();
}

/// `.time()` and its siblings take an unsigned `INT_P` precision (unlike
/// `.decimal()`'s signed arguments).
#[test]
fn jsonpath_datetime_methods_reject_a_negative_precision() {
    let db = setup();
    for m in ["time", "time_tz", "timestamp", "timestamp_tz"] {
        assert_first_line!(
            db.analyze(&format!("SELECT '$.{m}(-1)'::jsonpath")),
            r#"syntax error at or near "-" of jsonpath input"#
        );
    }
    assert_first_line!(
        db.analyze("SELECT '$.time(+1)'::jsonpath"),
        r#"syntax error at or near "+" of jsonpath input"#
    );
    db.analyze("SELECT '$.time(1)'::jsonpath AS v, '$.decimal(-1, +2)'::jsonpath AS w")
        .unwrap();
}

/// The scanner raises a malformed `E'\u'` escape as
/// invalid_escape_sequence (22025); the parser words a malformed `U&''`
/// escape the same, as a syntax error (42601).
#[test]
fn invalid_unicode_escape_is_an_invalid_escape_sequence() {
    let db = setup();
    for sql in [r"SELECT E'wrong: \U0061'", r"SELECT E'\u006'"] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            err.to_string().starts_with("invalid Unicode escape"),
            "{sql}: {err}"
        );
        assert!(
            matches!(err, AnalyzeError::InvalidEscapeSequence(_)),
            "{sql}: PG's SQLSTATE is 22025 (invalid_escape_sequence), got {err:?}"
        );
    }
    let err = db.analyze(r"SELECT U&'\zz'").unwrap_err();
    assert!(
        err.to_string().starts_with("invalid Unicode escape"),
        "{err}"
    );
    assert!(matches!(err, AnalyzeError::Parse(_)), "{err:?}");
}

#[test]
fn cidr_literal_host_bits_rejected() {
    let db = setup();
    for lit in ["1.2.3.4/24", "10.1/8", "fe80::1/64"] {
        assert_first_line!(
            db.analyze(&format!("SELECT '{lit}'::cidr")),
            &format!("invalid cidr value: \"{lit}\"")
        );
    }
    for q in [
        "SELECT '1.2.3.0/24'::cidr AS v",
        "SELECT '10'::cidr AS v",
        "SELECT 'fe80::/64'::cidr AS v",
        // inet takes an abbreviated address when the mask covers it.
        "SELECT '192.168/16'::inet AS v",
    ] {
        db.analyze(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

/// An explicit cast hands its typmod to `interval_in`, whose field
/// restriction decides how bare numbers and `mm:ss` fields decode — so a
/// cast's interval literal is decided exactly, not only when every
/// restriction agrees.
#[test]
fn interval_cast_literal_uses_the_written_field_restriction() {
    let db = setup();
    assert_first_line!(
        db.analyze("SELECT '1 1'::interval"),
        "invalid input syntax for type interval: \"1 1\""
    );
    assert_first_line!(
        db.analyze("SELECT '[1,2)'::interval"),
        "invalid input syntax for type interval: \"[1,2)\""
    );
    db.analyze("SELECT '1 1'::interval day to hour AS v")
        .unwrap();
    db.analyze("SELECT '[1,2)'::interval hour AS v").unwrap();
    db.analyze("SELECT '1:30'::interval minute to second AS v")
        .unwrap();
}

/// AdjustIntervalForTypmod rounds the time to the typmod's precision,
/// which overflows next to the int64 limit.
#[test]
fn interval_typmod_rounding_checks_the_range() {
    let db = setup();
    for sql in [
        "SELECT interval '2562047788:00:54.775807' second(2)",
        "SELECT interval '-2562047788:00:54.775807' second(2)",
        "SELECT interval '2562047788:00:54.775806' second(0)",
    ] {
        assert_first_line!(db.analyze(sql), "interval out of range");
    }
    for sql in [
        "SELECT interval '2562047788:00:54.775807' second(6) AS v",
        "SELECT interval '2562047788:00:54.775807' AS v",
        "SELECT interval '2562047788:00:54.775807' hour AS v",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// An assigned literal is read by interval_in with the target column's
/// typmod, so the column's field restriction decides the decoding.
#[test]
fn interval_assignment_literal_uses_the_column_field_restriction() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE iv (id INT PRIMARY KEY, h INTERVAL HOUR, dh INTERVAL DAY TO HOUR,
                          plain INTERVAL);",
    )
    .unwrap();
    for sql in [
        "INSERT INTO iv (id, h) VALUES (1, '[1,2)')",
        "INSERT INTO iv (id, dh) VALUES (1, '1 1')",
        "UPDATE iv SET dh = '1 1'",
        "INSERT INTO iv (id, dh) SELECT 1, '1 1'",
        "MERGE INTO iv USING (SELECT 1 AS id) s ON iv.id = s.id \
         WHEN MATCHED THEN UPDATE SET dh = '1 1'",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for sql in [
        "INSERT INTO iv (id, plain) VALUES (1, '1 1')",
        "UPDATE iv SET plain = '1 1'",
        "INSERT INTO iv (id, plain) SELECT 1, '1 1'",
        "MERGE INTO iv USING (SELECT 1 AS id) s ON iv.id = s.id \
         WHEN MATCHED THEN UPDATE SET plain = '1 1'",
    ] {
        assert_first_line!(
            db.analyze(sql),
            "invalid input syntax for type interval: \"1 1\""
        );
    }
}

/// The reg* input functions parse a (qualified) name with PG's identifier
/// rules — or, for regtype, a whole type name with the grammar — and look
/// it up at parse time.
#[test]
fn reg_type_literals_are_parsed_and_looked_up() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT '1.5e3'::regclass",
            "relation \"1.5e3\" does not exist",
        ),
        (
            "SELECT '{1,2}'::regclass",
            "relation \"{1,2}\" does not exist",
        ),
        ("SELECT 'x y'::regclass", "invalid name syntax"),
        (
            "SELECT 'a.b.c.d'::regclass",
            "improper relation name (too many dotted names): a.b.c.d",
        ),
        ("SELECT '{1,2}'::regtype", "syntax error at or near \"{\""),
        (
            "SELECT '1.5e3'::regtype",
            "syntax error at or near \"1.5e3\"",
        ),
        (
            "SELECT 'nosuch[]'::regtype",
            "type \"nosuch[]\" does not exist",
        ),
        // An array type has no array type of its own.
        (
            "SELECT '_int4[]'::regtype",
            "type \"_int4[]\" does not exist",
        ),
        (
            "SELECT CAST(NULL AS _int4[])",
            "type \"_int4[]\" does not exist",
        ),
        ("SELECT NULL::nosuch[]", "type \"nosuch[]\" does not exist"),
        (
            "SELECT 'setof int'::regtype",
            "invalid type name \"setof int\"",
        ),
        (
            "SELECT '1.5e3'::regproc",
            "function \"1.5e3\" does not exist",
        ),
        ("SELECT '1.5e3'::regnamespace", "invalid name syntax"),
        ("SELECT '1.5e3'::regoper", "operator does not exist: 1.5e3"),
        (
            "SELECT '1.5e3'::regprocedure",
            "expected a left parenthesis",
        ),
    ] {
        assert_first_line!(db.analyze(sql), msg);
    }
    for sql in [
        "SELECT ' T '::regclass AS v",
        "SELECT 'public.t'::regclass AS v",
        "SELECT 'int[]'::regtype AS v, 'numeric(10,2)'::regtype AS w",
        "SELECT 'character varying'::regtype AS v",
        "SELECT '-'::regclass AS v",
        "SELECT 'lower(text)'::regprocedure AS v",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// Every coercion of an untyped literal runs the target's input function —
/// in COALESCE, a comparison, an INSERT value or an array comparison as in
/// an explicit cast.
#[test]
fn malformed_literals_rejected_in_implicit_coercions() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE posts (id INT PRIMARY KEY, published_at TIMESTAMPTZ, tags TEXT[]);")
        .unwrap();
    for sql in [
        "SELECT COALESCE(published_at, 'a0-01-01') FROM posts",
        "SELECT id FROM posts WHERE published_at = 'a0-01-01'",
        "INSERT INTO posts (id, published_at) VALUES (1, 'a0-01-01')",
        "SELECT '2024-01-01'::timetz",
    ] {
        let err = db.analyze(sql).expect_err(sql).to_string();
        assert!(
            err.starts_with("invalid input syntax for type"),
            "{sql}: {err}"
        );
    }
    assert_first_line!(
        db.analyze("SELECT id FROM posts WHERE tags = '{\"a\": 1}'"),
        "malformed array literal: \"{\"a\": 1}\""
    );
}

/// coerce_type hands interval_in the target's typmod: `-1` for operator
/// and function arguments and common types, the domain's for a domain, a
/// subscripted assignment's container typmod — but `array_in` gets `-1`,
/// so array elements decode unrestricted. An interval literal is decided
/// exactly everywhere.
#[test]
fn interval_literals_decode_with_the_coercion_typmod() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN dh AS INTERVAL DAY TO HOUR;
         CREATE TABLE ivs (id INT PRIMARY KEY, d dh, a INTERVAL DAY TO HOUR[], p INTERVAL);",
    )
    .unwrap();
    for sql in [
        "SELECT p FROM ivs WHERE p = '10: 0'",
        "SELECT p FROM ivs WHERE p = '1 1'",
        "SELECT COALESCE(p, '1 1') FROM ivs",
        "INSERT INTO ivs (id, a) VALUES (1, '{\"1 1\"}')",
    ] {
        assert_first_line!(db.analyze(sql), "invalid input syntax for type interval");
    }
    for sql in [
        "SELECT '1 1'::dh AS v",
        "INSERT INTO ivs (id, d) VALUES (1, '1 1')",
        "UPDATE ivs SET a[1] = '1 1'",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
