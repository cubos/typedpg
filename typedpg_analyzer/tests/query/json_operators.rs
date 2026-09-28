//! JSON / JSONB operators beyond `->>` and `?` (which are already covered
//! in [`special.rs`](special.rs)): containment `@>` / `<@`, key-exists
//! variants `?|` / `?&`, path operators `#>` / `#>>`, constructors
//! `jsonb_build_object` / `json_build_array`.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            prefs JSONB,
            meta  JSONB NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── Containment: @>, <@ ──────────────────────────────────────────────────────

#[test]
fn jsonb_contains_rhs_literal() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE prefs @> '{\"a\":1}'::jsonb")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn jsonb_contained_by_rhs_literal() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE '{\"a\":1}'::jsonb <@ meta")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn jsonb_contains_param() {
    let db = setup();
    // `meta @> $p1` — param is inferred as `jsonb` from the left-hand column.
    let s = db
        .analyze("SELECT id FROM users WHERE meta @> $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(jsonb())]);
}

// ── Key-exists variants: ?|, ?& ──────────────────────────────────────────────

#[test]
fn jsonb_any_key_exists() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE prefs ?| ARRAY['a','b']")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn jsonb_all_keys_exist() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE meta ?& ARRAY['a','b']")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── Path extraction: #>, #>> ─────────────────────────────────────────────────

#[test]
fn jsonb_path_get_text() {
    let db = setup();
    // `#>>` returns text. Nullable because the path may not exist.
    let s = db
        .analyze("SELECT prefs #>> '{a,b}' AS v FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("v", text())]);
}

#[test]
fn jsonb_path_get_jsonb() {
    let db = setup();
    // `#>` returns jsonb. Nullable: path may not exist even on NOT NULL meta.
    let s = db
        .analyze("SELECT meta #> '{a,b}' AS v FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("v", jsonb())]);
}

// ── Constructors ─────────────────────────────────────────────────────────────

#[test]
fn jsonb_build_object_returns_jsonb() {
    let db = setup();
    // Non-strict builder: NULL arguments produce a JSON `null` entry, so the
    // outer value is never SQL NULL.
    let s = db
        .analyze("SELECT jsonb_build_object('k', 1) AS x")
        .unwrap();
    assert_cols(&s, vec![c("x", jsonb())]);
}

#[test]
fn json_build_array_returns_json() {
    let db = setup();
    let s = db.analyze("SELECT json_build_array(1, 'a') AS x").unwrap();
    assert_cols(&s, vec![c("x", json_ty())]);
}

// ── jsonpath operators: @?, @@ ──────────────────────────────────────────────

#[test]
fn jsonb_jsonpath_exists_returns_bool() {
    let db = setup();
    // `@?` takes (jsonb, jsonpath) and returns bool. NOT NULL because the
    // operator is strict on a NOT NULL `meta`.
    let s = db
        .analyze("SELECT meta @? '$.a'::jsonpath AS exists FROM users")
        .unwrap();
    assert_cols(&s, vec![c("exists", bool_ty())]);
}

#[test]
fn jsonb_jsonpath_match_returns_bool() {
    let db = setup();
    // `@@` takes (jsonb, jsonpath) and returns bool.
    let s = db
        .analyze("SELECT meta @@ '$.a > 0'::jsonpath AS m FROM users")
        .unwrap();
    assert_cols(&s, vec![c("m", bool_ty())]);
}

#[test]
fn jsonb_jsonpath_exists_nullable_lhs_propagates() {
    let db = setup();
    // `prefs @? '$.a'` — strict, prefs nullable → result nullable.
    let s = db
        .analyze("SELECT prefs @? '$.a'::jsonpath AS exists FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("exists", bool_ty())]);
}

// ── jsonb_set / jsonb_insert ────────────────────────────────────────────────

#[test]
fn jsonb_set_returns_jsonb() {
    let db = setup();
    // `jsonb_set(jsonb, text[], jsonb)` — strict, NOT NULL `meta` → NOT NULL.
    let s = db
        .analyze("SELECT jsonb_set(meta, '{a}', '1'::jsonb) AS m FROM users")
        .unwrap();
    assert_cols(&s, vec![c("m", jsonb())]);
}

#[test]
fn jsonb_set_with_create_missing_flag() {
    let db = setup();
    // 4-arg form `jsonb_set(jsonb, text[], jsonb, bool)`. Same return type.
    let s = db
        .analyze("SELECT jsonb_set(meta, '{a}', '1'::jsonb, true) AS m FROM users")
        .unwrap();
    assert_cols(&s, vec![c("m", jsonb())]);
}

#[test]
fn jsonb_set_on_nullable_lhs() {
    let db = setup();
    // Nullable lhs propagates to nullable output.
    let s = db
        .analyze("SELECT jsonb_set(prefs, '{a}', '1'::jsonb) AS m FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("m", jsonb())]);
}

#[test]
fn jsonb_insert_returns_jsonb() {
    let db = setup();
    // `jsonb_insert(jsonb, text[], jsonb)` — strict on NOT NULL `meta`.
    let s = db
        .analyze("SELECT jsonb_insert(meta, '{a}', '1'::jsonb) AS m FROM users")
        .unwrap();
    assert_cols(&s, vec![c("m", jsonb())]);
}

#[test]
fn jsonb_insert_with_after_flag() {
    let db = setup();
    // 4-arg form with the `insert_after` boolean.
    let s = db
        .analyze("SELECT jsonb_insert(meta, '{a}', '1'::jsonb, true) AS m FROM users")
        .unwrap();
    assert_cols(&s, vec![c("m", jsonb())]);
}

#[test]
fn jsonb_set_lax_two_arg_defaults() {
    let db = setup();
    // `jsonb_set_lax` has 2 trailing default args (`create_if_missing` and
    // `null_value_treatment`); the 3-required-arg call must resolve.
    // `jsonb_set_lax` is non-strict, but its only NULL results come from
    // NULL arguments, so non-NULL arguments give a non-NULL result.
    let s = db
        .analyze("SELECT jsonb_set_lax(meta, '{a}', '1'::jsonb) AS m FROM users")
        .unwrap();
    assert_cols(&s, vec![c("m", jsonb())]);
}

// ── Subscripting: data['key'], data[0], chained ─────────────────────────────

#[test]
fn jsonb_subscript_by_key_returns_jsonb() {
    let db = setup();
    // `prefs['theme']` — subscript yields jsonb, always nullable (a missing
    // key produces NULL).
    let s = db.analyze("SELECT prefs['theme'] AS v FROM users").unwrap();
    assert_cols(&s, vec![cn("v", jsonb())]);
}

#[test]
fn jsonb_subscript_on_not_null_column_is_nullable() {
    let db = setup();
    // Even on the NOT NULL `meta` column, a subscript is nullable — the key
    // may not exist.
    let s = db.analyze("SELECT meta['a'] AS v FROM users").unwrap();
    assert_cols(&s, vec![cn("v", jsonb())]);
}

#[test]
fn jsonb_subscript_chained() {
    let db = setup();
    // `meta['a']['b']` — chained subscripts keep yielding jsonb, nullable.
    let s = db.analyze("SELECT meta['a']['b'] AS v FROM users").unwrap();
    assert_cols(&s, vec![cn("v", jsonb())]);
}

#[test]
fn jsonb_subscript_by_numeric_index() {
    let db = setup();
    // `meta[0]` — numeric index into a jsonb array also yields jsonb.
    let s = db.analyze("SELECT meta[0] AS v FROM users").unwrap();
    assert_cols(&s, vec![cn("v", jsonb())]);
}

#[test]
fn jsonb_subscript_in_where_clause() {
    let db = setup();
    // Subscript in WHERE: `prefs['theme']` resolves to jsonb so the
    // comparison against a jsonb literal type-checks.
    let s = db
        .analyze("SELECT id FROM users WHERE prefs['theme'] = '\"dark\"'::jsonb")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn jsonb_slice_subscript_is_rejected() {
    let db = setup();
    // PG's jsonb subscript handler rejects slices — error message matches
    // PG verbatim.
    assert_analyze_err!(
        db.analyze("SELECT meta['a':'b'] AS v FROM users"),
        AnalyzeError::DatatypeMismatch(_),
        "jsonb subscript does not support slices",
    );
}

#[test]
fn json_subscript_is_rejected() {
    let db = setup();
    // Only `jsonb` has a subscript handler in PG; the plain `json` type
    // does not, so subscripting it is an error — message matches PG.
    assert_analyze_err!(
        db.analyze("SELECT ('{\"a\":1}'::json)['a'] AS v"),
        AnalyzeError::DatatypeMismatch(_),
        "cannot subscript type json because it does not support subscripting",
    );
}

// ── jsonb subscripts (PG's jsonb_subscript_transform) ───────────────────────

fn jsonb_subscript_db() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id int PRIMARY KEY, j jsonb NOT NULL, js json);")
        .unwrap();
    db
}

#[test]
fn jsonb_subscript_accepts_int_and_text_coercible_keys() {
    let db = jsonb_subscript_db();
    let s = db
        .analyze("SELECT j['a'] AS i, j[1::int2] AS k, j['a'::varchar] AS l, j[NULL] AS m FROM t")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("i", jsonb()),
            cn("k", jsonb()),
            cn("l", jsonb()),
            cn("m", jsonb()),
        ],
    );
    // An untyped subscript becomes text, a typed int an array index.
    let s = db
        .analyze("SELECT j[$k] AS a, j[$i + 1] AS b FROM t")
        .unwrap();
    assert_params(&s, vec![p(text()), p(int4())]);
}

#[test]
fn jsonb_subscript_rejects_other_key_types() {
    let db = jsonb_subscript_db();
    for (sql, ty) in [
        ("SELECT j[true] AS a FROM t", "boolean"),
        ("SELECT j[1.5] AS a FROM t", "numeric"),
        ("SELECT j[1::int8] AS a FROM t", "bigint"),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DatatypeMismatch(_),
            &format!("subscript type {ty} is not supported")
        );
    }
}

#[test]
fn jsonb_subscript_slices_rejected_with_42804() {
    let db = jsonb_subscript_db();
    for sql in [
        "SELECT j['a':'b'] FROM t",
        "SELECT j['a'][1:2] FROM t",
        "SELECT j[1:] FROM t",
        "UPDATE t SET j[1:2] = '1'",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DatatypeMismatch(_),
            "jsonb subscript does not support slices"
        );
    }
    assert_err_prefix!(
        db.analyze("UPDATE t SET j[true] = '1'"),
        AnalyzeError::DatatypeMismatch(_),
        "subscript type boolean is not supported"
    );
}

#[test]
fn json_is_not_subscriptable_42804() {
    let db = jsonb_subscript_db();
    assert_err_prefix!(
        db.analyze("SELECT js['a'] FROM t"),
        AnalyzeError::DatatypeMismatch(_),
        "cannot subscript type json because it does not support subscripting"
    );
}

#[test]
fn update_jsonb_subscript_assigns_jsonb() {
    let db = jsonb_subscript_db();
    db.analyze("UPDATE t SET j['a'] = '1'").unwrap();
    let s = db.analyze("UPDATE t SET j['a'] = $v").unwrap();
    assert_params(&s, vec![pn(jsonb())]);
    assert_err_prefix!(
        db.analyze("UPDATE t SET j.a = '1'"),
        AnalyzeError::DatatypeMismatch(_),
        "cannot assign to field \"a\" of column \"j\" because its type jsonb is not a composite type"
    );
}

// ── jsonpath literals (PG's jsonpath_in) ────────────────────────────────────

#[test]
fn invalid_jsonpath_literals_rejected() {
    // jsonpath_in runs when the unknown literal is coerced to jsonpath, both
    // for an explicit cast and for an operator argument (verified on PG 18).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id int PRIMARY KEY, j jsonb NOT NULL);")
        .unwrap();
    for (sql, msg) in [
        (
            "SELECT j @? 'x' AS a FROM t",
            "syntax error at end of jsonpath input",
        ),
        (
            "SELECT '$.a.bad('::jsonpath AS p",
            "syntax error at or near \"(\" of jsonpath input",
        ),
        (
            "SELECT jsonb_path_query(j, '$.a ? (@.b == 1 == 2)') FROM t",
            "syntax error at or near \"==\" of jsonpath input",
        ),
        (
            "SELECT '@.a'::jsonpath",
            "@ is not allowed in root expressions",
        ),
        (
            "SELECT '$.a ? (last == 1)'::jsonpath",
            "LAST is allowed only in array subscripts",
        ),
        (
            "SELECT '1a'::jsonpath",
            "trailing junk after numeric literal at or near \"1a\" of jsonpath input",
        ),
        (
            "SELECT ''::jsonpath",
            "invalid input syntax for type jsonpath: \"\"",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::InvalidLiteral(_), msg);
    }
}

#[test]
fn valid_jsonpath_literals_accepted() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id int PRIMARY KEY, j jsonb NOT NULL);")
        .unwrap();
    let s = db
        .analyze(
            "SELECT j @? '$.a[*] ? (@.b > 1 && @.c like_regex \"^x\" flag \"i\")' AS a, \
             j @@ 'strict $.a.size() == 2' AS b, \
             '$.**{1 to last}.datetime(\"yyyy\")'::jsonpath AS c FROM t",
        )
        .unwrap();
    assert_col!(&s, "c", basic("pg_catalog", "jsonpath"), nullable = false);
}
