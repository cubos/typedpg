//! The "cannot error" proof the NOT IN and count rules rely on: which
//! casts, functions and operators it accepts in a counted query or next to
//! a rewrite site. Under `pg_sanity` every rewrite is also executed next to
//! the original on the adversarial rows.

use crate::common::*;
use typedpg_analyzer::Rule;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
         CREATE DOMAIN posint AS int CHECK (VALUE > 0);
         CREATE TABLE customers (id bigint PRIMARY KEY, name text NOT NULL,
                                 email varchar NOT NULL, code char(3) NOT NULL,
                                 mood mood NOT NULL);
         CREATE TABLE orders (id bigint PRIMARY KEY, customer_id bigint NOT NULL,
                              qty smallint NOT NULL, n int NOT NULL, total numeric NOT NULL,
                              ratio real NOT NULL, score float8 NOT NULL,
                              created_at timestamptz NOT NULL, placed_on date NOT NULL,
                              at_local timestamp NOT NULL, note text, paid bool NOT NULL,
                              p posint NOT NULL);
         CREATE TABLE banned (customer_id bigint NOT NULL, email text NOT NULL);",
    )
    .unwrap();
    db
}

/// `cond` (over `table`'s columns) in a counted query: whether the count
/// is rewritten to `EXISTS`.
#[track_caller]
fn counted(db: &PgCatalog, table: &str, cond: &str) -> bool {
    let sql = format!("SELECT (SELECT count(*) FROM {table} WHERE {cond}) > 0 AS x");
    let a = db.analyze(&sql).unwrap();
    if a.simplifications.is_empty() {
        return false;
    }
    assert_eq!(
        a.sql,
        format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE {cond}) AS x"),
        "for {sql}"
    );
    assert!(
        a.simplifications
            .iter()
            .all(|s| s.rule == Rule::CountExists)
    );
    true
}

/// `cond` (over `orders o`) next to a NOT IN in the WHERE: whether the
/// NOT IN is rewritten.
#[track_caller]
fn beside_not_in(db: &PgCatalog, cond: &str) -> bool {
    let sql = format!(
        "SELECT o.id FROM orders o \
         WHERE o.customer_id NOT IN (SELECT customer_id FROM banned) AND {cond}"
    );
    let a = db.analyze(&sql).unwrap();
    if a.simplifications.is_empty() {
        return false;
    }
    assert_eq!(
        a.sql,
        format!(
            "SELECT o.id FROM orders o \
             WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE o.customer_id = customer_id)) \
             AND {cond}"
        ),
        "for {sql}"
    );
    true
}

#[track_caller]
fn assert_proved(db: &PgCatalog, table: &str, conds: &[&str]) {
    for cond in conds {
        assert!(counted(db, table, cond), "count kept for {cond}");
        if table == "orders" {
            assert!(beside_not_in(db, cond), "NOT IN kept beside {cond}");
        }
    }
}

#[track_caller]
fn assert_refused(db: &PgCatalog, table: &str, conds: &[&str]) {
    for cond in conds {
        assert!(!counted(db, table, cond), "count rewritten for {cond}");
        if table == "orders" {
            assert!(!beside_not_in(db, cond), "NOT IN rewritten beside {cond}");
        }
    }
}

// ── Casts ───────────────────────────────────────────────────────────────────

/// Implicit casts PG inserts to reach an operator's argument types, when
/// they can't fail: integer widening, integer to numeric or float, float4
/// to float8, a domain read as its base type.
#[test]
fn implicit_casts_that_cannot_fail() {
    assert_proved(
        &setup(),
        "orders",
        &[
            // int4 -> numeric, on the constant (the issue's own query).
            "total > 10",
            // int4 -> numeric, on the column.
            "n > 2.5",
            // int8 -> numeric (a literal past int8 is numeric).
            "customer_id < 99999999999999999999",
            // int4 / int8 -> float8.
            "score > n",
            "score > 9223372036854775807",
            // int2 -> numeric / float8.
            "qty < 2.5",
            "qty < score",
            // float4 -> float8 (or the cross-type operator).
            "ratio > score",
            "ratio > n",
            // A domain over int4.
            "p > 0",
        ],
    );
}

#[test]
fn binary_relabels_and_text_casts() {
    let db = setup();
    assert_proved(
        &db,
        "customers",
        &[
            // varchar -> text, relabeled.
            "email = 'a@b.c'",
            "email = name",
            // bpchar -> text through rtrim1.
            "code::text = name",
            // Written casts that can't fail.
            "id::text = '1'",
            "id::numeric > 1",
            "mood = 'ok'",
            "mood > 'sad'",
        ],
    );
    assert_proved(
        &db,
        "orders",
        &[
            "n::bigint > 1",
            "paid::text = 'true'",
            "total::text <> ''",
            "qty::float8 > 1",
        ],
    );
}

/// Casts that fail on some value: narrowing, numeric or float to integer,
/// float8 to float4, numeric to float, date or timestamp to the other
/// kind of timestamp, text parsing, a length coercion, a domain's CHECK.
#[test]
fn casts_that_can_fail_are_refused() {
    let db = setup();
    assert_refused(
        &db,
        "orders",
        &[
            "n::smallint > 1",
            "customer_id::int > 1",
            "total::int > 1",
            "score::int > 1",
            "score::real > 1",
            // numeric -> float8, implicit: `'1e400'::numeric::float8`.
            "score > total",
            "total::float8 > 1",
            // date -> timestamp(tz), timestamp <-> timestamptz.
            "placed_on::timestamptz > created_at",
            "placed_on::timestamp > at_local",
            "at_local::timestamptz > created_at",
            "created_at::timestamp > at_local",
            "created_at::date > placed_on",
            // Parsing text.
            "note::int > 0",
            "note::timestamptz > created_at",
            // A length or precision coercion.
            "note::varchar(2) = 'ab'",
            "total::numeric(3, 1) > 1",
            // A domain's CHECK.
            "n::posint > 0",
            // An array coercion.
            "ARRAY[n]::bigint[] = ARRAY[1::bigint]",
        ],
    );
}

// ── Functions ───────────────────────────────────────────────────────────────

#[test]
fn functions_that_cannot_fail() {
    let db = setup();
    assert_proved(
        &db,
        "orders",
        &[
            "lower(note) = 'x'",
            "upper(note) <> 'X'",
            "length(note) > 3",
            "char_length(note) > 3 AND octet_length(note) < 10",
            "abs(score) < 1",
            "abs(ratio) < 1",
            "abs(total) > 1",
            "coalesce(note, '') <> ''",
            "coalesce(n, 0) > 1.5",
            "created_at < now()",
            "created_at < statement_timestamp()",
            "created_at < current_timestamp",
            "placed_on <= current_date",
            "at_local <= localtimestamp",
            "created_at > current_date",
        ],
    );
    assert_proved(
        &db,
        "customers",
        &["lower(email) = lower('A@B.C')", "length(code) = 3"],
    );
}

#[test]
fn functions_that_can_fail_are_refused() {
    let db = setup();
    assert_refused(
        &db,
        "orders",
        &[
            // `abs(-2147483648)` overflows.
            "abs(n) > 1",
            "abs(customer_id) > 1",
            // Negative substring length.
            "substr(note, 1, n) = 'x'",
            // `float8 -> numeric` is an assignment cast; the common type
            // of numeric and float8 is float8: numeric -> float8 may fail.
            "coalesce(total, score) > 0",
            // Volatile, and not on the list.
            "created_at < clock_timestamp()",
            // A precision rounds.
            "created_at < current_timestamp(3)",
            // A COLLATE clause isn't modelled.
            "lower(note COLLATE \"C\") = 'x'",
        ],
    );
}

// ── Clock arithmetic ────────────────────────────────────────────────────────

/// A clock reading moved by a bounded constant stays far inside the
/// range: `timestamptz - interval` can't overflow there.
#[test]
fn clock_shifted_by_a_constant() {
    assert_proved(
        &setup(),
        "orders",
        &[
            "created_at > now() - interval '1 day'",
            "created_at > current_timestamp - interval '30 days'",
            "created_at > now() + '-2 hours 30 minutes'::interval",
            "created_at > transaction_timestamp() - interval '1 year 2 mons'",
            "at_local < localtimestamp + interval '1 hour'",
            "placed_on >= current_date - 7",
            "placed_on > current_date - interval '1 month'",
            "created_at > now() - interval '1000 years'",
        ],
    );
}

/// Arithmetic that may leave the range: an unbounded shift (a parameter,
/// a column, a huge constant), a shift of something other than the clock.
#[test]
fn unbounded_date_arithmetic_is_refused() {
    let db = setup();
    assert_refused(
        &db,
        "orders",
        &[
            "created_at > now() - interval '300000 years'",
            "created_at > now() - interval '1001 years'",
            "created_at > now() - $since::interval",
            "created_at > now() - n * interval '1 day'",
            "placed_on > current_date - 100000000",
            "placed_on > current_date - n",
            "created_at > created_at - interval '1 day'",
            "at_local > at_local + interval '1 day'",
            "created_at > clock_timestamp() - interval '1 day'",
            "created_at > now() - interval 'infinity'",
            // Plain arithmetic overflows.
            "n + 1 > 2",
            "total * 2 > 2",
        ],
    );
}

// ── Operators ───────────────────────────────────────────────────────────────

#[test]
fn between_in_and_distinct() {
    let db = setup();
    assert_proved(
        &db,
        "orders",
        &[
            "n BETWEEN 1 AND 10.5",
            "placed_on BETWEEN '2024-01-01' AND current_date",
            "n NOT BETWEEN 1 AND 10",
            "n IN (1, 2, 3)",
            "n NOT IN (1, 2)",
            "total IN (1, 2.5)",
            "note IN ('a', 'b')",
            "n IN (1)",
            "customer_id IS DISTINCT FROM 3",
            "customer_id IS NOT DISTINCT FROM 3",
        ],
    );
    assert_proved(
        &db,
        "customers",
        &["mood IN ('ok', 'happy')", "email IN ('a', 'b')"],
    );
    assert_refused(
        &db,
        "orders",
        &[
            // An item with a column is compared on its own: kept simple.
            "n IN (1, qty)",
            "n BETWEEN 1 AND score::int",
            "n IN (1, 2) OR n::smallint = 1",
        ],
    );
}
