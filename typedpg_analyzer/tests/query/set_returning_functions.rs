//! Set-returning functions: element nullability, lockstep padding in the
//! select list, and function RTEs in FROM (`ROWS FROM`, column definition
//! lists, composite-returning functions).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, arr int[] NOT NULL, s text NOT NULL);
         CREATE TABLE n (id int PRIMARY KEY, x int);",
    )
    .unwrap();
    db
}

// ── Element nullability ──────────────────────────────────────────────────────

/// PG 18: with `arr = '{1,NULL}'`, `unnest(arr) IS NULL` is true for the
/// second row — a NOT NULL array column can still hold NULL elements.
#[test]
fn unnest_of_not_null_array_column_is_nullable() {
    let db = setup();
    let s = db.analyze("SELECT unnest(arr) FROM t").unwrap();
    assert_cols(&s, vec![cn("unnest", int4())]);
    let s = db.analyze("SELECT u FROM t, unnest(arr) AS u(u)").unwrap();
    assert_cols(&s, vec![cn("u", int4())]);
    let s = db
        .analyze("SELECT v FROM t CROSS JOIN LATERAL unnest(t.arr) AS u(v)")
        .unwrap();
    assert_cols(&s, vec![cn("v", int4())]);
}

#[test]
fn unnest_of_array_literal_with_null_is_nullable() {
    let db = setup();
    let s = db.analyze("SELECT unnest(ARRAY[1, NULL])").unwrap();
    assert_cols(&s, vec![cn("unnest", int4())]);
    let s = db
        .analyze("SELECT * FROM unnest(ARRAY[1, NULL]) u")
        .unwrap();
    assert_cols(&s, vec![cn("u", int4())]);
}

/// An `ARRAY[…]` constructor whose elements are all NOT NULL provably
/// unnests to NOT NULL elements.
#[test]
fn unnest_of_not_null_array_constructor_stays_not_null() {
    let db = setup();
    let s = db.analyze("SELECT unnest(ARRAY[1, 2])").unwrap();
    assert_cols(&s, vec![c("unnest", int4())]);
    let s = db
        .analyze("SELECT e FROM t, unnest(ARRAY[t.id, 3]) AS u(e)")
        .unwrap();
    assert_cols(&s, vec![c("e", int4())]);
}

/// `json[b]_array_elements_text` maps a JSON `null` element to SQL NULL
/// (PG 18: `x IS NULL` is true for `'[null]'`); `jsonb_array_elements`
/// yields the JSON value `null`, which is not SQL NULL.
#[test]
fn json_array_elements_text_is_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT jsonb_array_elements_text('[null]'), jsonb_array_elements('[null]')")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("jsonb_array_elements_text", text()),
            cn("jsonb_array_elements", jsonb()),
        ],
    );
    let s = db
        .analyze("SELECT * FROM jsonb_array_elements('[null]') AS x(v)")
        .unwrap();
    assert_cols(&s, vec![c("v", jsonb())]);
}

/// A strict SRF is not called for a NULL argument — PG returns zero rows
/// (`SELECT generate_series(1, NULL::int)` is empty), so its elements stay
/// NOT NULL even when an argument is nullable.
#[test]
fn strict_srf_with_nullable_argument_is_not_null() {
    let db = setup();
    let s = db.analyze("SELECT generate_series(1, x) FROM n").unwrap();
    assert_cols(&s, vec![c("generate_series", int4())]);
    let s = db
        .analyze("SELECT g FROM n, generate_series(1, n.x) AS g")
        .unwrap();
    assert_cols(&s, vec![c("g", int4())]);
}

// ── Lockstep padding ─────────────────────────────────────────────────────────

/// PG 18: `unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c'])` row 3 is `(NULL, 'c')`.
#[test]
fn multi_arg_unnest_pads_with_null() {
    let db = setup();
    let s = db
        .analyze("SELECT * FROM unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c'])")
        .unwrap();
    assert_cols(&s, vec![cn("unnest", int4()), cn("unnest", text())]);
}

/// PG 18: `SELECT generate_series(1, 3) a, generate_series(1, 2) b` row 3 is
/// `(3, NULL)` — select-list SRFs run in lockstep (ProjectSet).
#[test]
fn select_list_srfs_in_lockstep_are_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT generate_series(1, 3) a, generate_series(1, 2) b")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", int4())]);
    let s = db
        .analyze("SELECT unnest(ARRAY[1, 2]), unnest(ARRAY['a','b','c'])")
        .unwrap();
    assert_cols(&s, vec![cn("unnest", int4()), cn("unnest", text())]);
    let s = db
        .analyze("SELECT generate_series(1, 3) + 1 AS a, generate_series(1, 2) AS b")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", int4())]);
}

// ── Composite-returning functions in FROM ────────────────────────────────────

/// A row-type value does not enforce its table's NOT NULL constraints —
/// PG 18: `SELECT * FROM jsonb_populate_record(NULL::t, '{}')` returns
/// `id IS NULL` and `s IS NULL` both true.
#[test]
fn composite_srf_columns_ignore_table_not_null() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE r (id int PRIMARY KEY, s text NOT NULL);
         CREATE FUNCTION f_setof_r() RETURNS SETOF r AS $$ SELECT NULL::int, NULL::text $$ LANGUAGE sql;",
    )
    .unwrap();
    let s = db
        .analyze("SELECT * FROM jsonb_populate_record(NULL::r, '{}')")
        .unwrap();
    assert_cols(&s, vec![cn("id", int4()), cn("s", text())]);
    let s = db
        .analyze("SELECT id, s FROM jsonb_populate_recordset(NULL::r, '[{}]')")
        .unwrap();
    assert_cols(&s, vec![cn("id", int4()), cn("s", text())]);
    let s = db.analyze("SELECT * FROM unnest(ARRAY[NULL::r])").unwrap();
    assert_cols(&s, vec![cn("id", int4()), cn("s", text())]);
    let s = db.analyze("SELECT * FROM f_setof_r()").unwrap();
    assert_cols(&s, vec![cn("id", int4()), cn("s", text())]);
}
