//! Equivalent spellings of one value infer the same: nullability and what
//! is known of its values (`refinement`). The lint's fixes rewrite a query
//! to an equivalent one (`CASE WHEN a IS NULL THEN b ELSE a END` to
//! `COALESCE(a, b)`, `COALESCE(n, 0) = 5` to `n = 5` where `n` is never
//! NULL, `x::int` to `x` on an integer, …), and a fix is offered only when
//! the generated types stay the same. Under `pg_sanity` every inference here
//! is also checked sound over seeded rows.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN small AS int CHECK (VALUE BETWEEN 0 AND 10);
         CREATE TABLE users (id bigint PRIMARY KEY, name text, st text NOT NULL,
                             active boolean, age int);
         CREATE TABLE payments (
             id bigint PRIMARY KEY,
             user_id bigint NOT NULL REFERENCES users,
             method text NOT NULL,
             card text,
             iban text,
             flag boolean,
             n int NOT NULL,
             vc varchar(10),
             lvl small,
             CHECK ((method = 'a' AND card IS NOT NULL AND iban IS NULL)
                 OR (method = 'b' AND iban IS NOT NULL AND card IS NULL)
                 OR method NOT IN ('a', 'b')),
             CHECK (num_nonnulls(card, iban) <= 1));",
    )
    .unwrap();
    db
}

/// The first output column's nullability and refinement.
fn shape(db: &PgCatalog, sql: &str) -> (bool, String) {
    let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let c = &s.columns[0];
    (c.nullable, format!("{:?}", c.refinement))
}

#[track_caller]
fn same(db: &PgCatalog, a: &str, b: &str) {
    assert_eq!(shape(db, a), shape(db, b), "\n  {a}\n  {b}");
}

#[test]
fn coalesce_reads_like_the_case_it_abbreviates() {
    let db = setup();
    for (from, filter) in [
        ("payments p", "p.method = 'b'"),
        ("payments p", "p.method = 'c'"),
        ("payments p", "true"),
        // The FOREIGN KEY finds every payment's user: `u.st IS NULL` keeps
        // no row, and neither spelling can be NULL.
        (
            "users u RIGHT JOIN payments p ON p.user_id = u.id",
            "p.method = 'c' AND u.st IS NULL",
        ),
    ] {
        same(
            &db,
            &format!(
                "SELECT CASE WHEN p.card IS NULL THEN p.iban ELSE p.card END AS k \
                 FROM {from} WHERE {filter}"
            ),
            &format!("SELECT COALESCE(p.card, p.iban) AS k FROM {from} WHERE {filter}"),
        );
    }
    // `method = 'b'` with `card` NULL leaves `iban` non-NULL.
    assert!(
        !shape(
            &db,
            "SELECT COALESCE(p.card, p.iban) AS k FROM payments p WHERE p.method = 'b'"
        )
        .0
    );
    // Proven NULL, the first argument is never the result: what is known
    // of the second is.
    same(
        &db,
        "SELECT CASE WHEN p.card IS NULL THEN 'x' ELSE p.card END AS k \
         FROM payments p WHERE p.method = 'b'",
        "SELECT COALESCE(p.card, 'x') AS k FROM payments p WHERE p.method = 'b'",
    );
}

#[test]
fn a_comparison_of_a_coalesce_says_what_its_column_is_when_not_null() {
    let db = setup();
    same(
        &db,
        "SELECT n FROM payments WHERE COALESCE(n, 0) = 5",
        "SELECT n FROM payments WHERE n = 5",
    );
    same(
        &db,
        "SELECT method FROM payments WHERE coalesce(method, '') = 'x'",
        "SELECT method FROM payments WHERE method = 'x'",
    );
    // A nullable column: its non-NULL values are 5, a NULL one makes the
    // COALESCE 0.
    assert_nullable_values(
        &db,
        "SELECT age FROM users WHERE COALESCE(age, 0) = 5",
        &[("age", false, Some(&["5"][..]))],
    );
    assert_nullable_values(
        &db,
        "SELECT age FROM users WHERE COALESCE(age, 5) = 5",
        &[("age", true, Some(&["5"][..]))],
    );
}

#[test]
fn identity_casts_read_like_their_operand() {
    let db = setup();
    same(
        &db,
        "SELECT n FROM payments WHERE (n::int)::int <= 3",
        "SELECT n FROM payments WHERE n <= 3",
    );
    same(
        &db,
        "SELECT n FROM payments WHERE n < ALL(ARRAY[0]::int[])",
        "SELECT n FROM payments WHERE n < ALL(ARRAY[0])",
    );
    same(
        &db,
        "SELECT CASE n::int WHEN 5 THEN n END AS c FROM payments WHERE n = 5",
        "SELECT CASE n WHEN 5 THEN n END AS c FROM payments WHERE n = 5",
    );
}

#[test]
fn more_spellings_lint_fixes_rewrite() {
    let db = setup();
    // A NULL test through an identity cast: `id` is the primary key, no
    // row passes, the column is never NULL either way.
    same(
        &db,
        "SELECT p.card FROM payments p WHERE (p.id)::bigint IS NULL",
        "SELECT p.card FROM payments p WHERE p.id IS NULL",
    );
    // A constant cast twice to its type.
    same(
        &db,
        "SELECT n FROM payments WHERE n::int < ('3'::int)::int",
        "SELECT n FROM payments WHERE n < '3'::int",
    );
    // A WHEN the planner folds to FALSE is never the CASE's value.
    same(
        &db,
        "SELECT COALESCE(CASE WHEN 1 >= 100 THEN true ELSE false END, flag) AS b FROM payments",
        "SELECT COALESCE((1 >= 100), flag) AS b FROM payments",
    );
    // A WHEN folding to TRUE leaves the ELSE out; a comparison of
    // constants is its value.
    same(
        &db,
        "SELECT CAST(CASE WHEN 'a' <> 'b' THEN true ELSE false END AS varchar) AS v",
        "SELECT CAST(('a' <> 'b') AS varchar) AS v",
    );
    // The CASE a COALESCE abbreviates says the same of its column.
    same(
        &db,
        "SELECT age FROM users WHERE CASE WHEN age IS NULL THEN 1 ELSE age END > 5",
        "SELECT age FROM users WHERE COALESCE(age, 1) > 5",
    );
    // No row passes (`addr > 'x'` keeps the users, `st IS NULL` none):
    // neither spelling can be anything.
    same(
        &db,
        "SELECT CASE WHEN p.iban IS NULL THEN 'x' ELSE p.iban END AS k \
         FROM users u FULL JOIN payments p ON p.user_id = u.id \
         WHERE u.name > 'x' AND u.st IS NULL",
        "SELECT COALESCE(p.iban, 'x') AS k \
         FROM users u FULL JOIN payments p ON p.user_id = u.id \
         WHERE u.name > 'x' AND u.st IS NULL",
    );
    // A scalar subquery is what its column is: a domain's bounds.
    same(
        &db,
        "SELECT (SELECT lvl FROM payments ORDER BY id LIMIT 1)::small AS l",
        "SELECT (SELECT lvl FROM payments ORDER BY id LIMIT 1) AS l",
    );
    // `WHERE 1 = 1` filters nothing: an aggregate over a source that
    // always has a row still has one.
    same(
        &db,
        "SELECT avg(v) AS a FROM unnest(ARRAY[5]) AS u(v) WHERE 1 = 1",
        "SELECT avg(v) AS a FROM unnest(ARRAY[5]) AS u(v)",
    );
    // `SET c = c` writes the value WHERE saw.
    same(
        &db,
        "UPDATE payments p SET n = p.n, card = NULL WHERE p.n >= 100 RETURNING p.n",
        "UPDATE payments p SET card = NULL WHERE p.n >= 100 RETURNING p.n",
    );
}

#[test]
fn a_decided_test_or_comparison_has_its_one_value() {
    let db = setup();
    same(
        &db,
        "SELECT CASE WHEN id IS NOT NULL THEN true ELSE false END AS b FROM payments",
        "SELECT (id IS NOT NULL) AS b FROM payments",
    );
    assert_nullable_values(
        &db,
        "SELECT (id IS NULL) AS b FROM payments",
        &[("b", false, Some(&["false"][..]))],
    );
    // A nullable value decides nothing.
    assert_nullable_values(
        &db,
        "SELECT (card IS NULL) AS b FROM payments",
        &[("b", false, None)],
    );
    same(
        &db,
        "SELECT CASE WHEN flag = true THEN true ELSE false END AS b FROM payments WHERE NOT flag",
        "SELECT CASE WHEN (flag = true) THEN true ELSE false END AS b FROM payments WHERE NOT flag",
    );
    assert_nullable_values(
        &db,
        "SELECT (flag = true) AS b FROM payments WHERE NOT flag",
        &[("b", false, Some(&["false"][..]))],
    );
}

#[test]
fn a_constant_a_cast_or_coalesce_leaves_as_it_is_fails_a_domain_check_too() {
    let db = setup();
    for value in ["100", "(100)::integer", "COALESCE(100, 0)"] {
        let sql = format!(
            "INSERT INTO payments (id, user_id, method, n, lvl) VALUES (1, 1, 'c', 1, {value})"
        );
        let err = db.analyze(&sql).unwrap_err().to_string();
        assert!(
            err.starts_with("value for domain small violates check constraint \"small_check\""),
            "{sql}: {err}"
        );
    }
}

/// Each column's (name, nullable, values).
#[track_caller]
fn assert_nullable_values(db: &PgCatalog, sql: &str, expected: &[(&str, bool, Option<&[&str]>)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, bool, Option<Vec<&str>>)> = s
        .columns
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.nullable,
                c.refinement
                    .values
                    .as_ref()
                    .map(|vs| vs.iter().map(String::as_str).collect()),
            )
        })
        .collect();
    let expected: Vec<(&str, bool, Option<Vec<&str>>)> = expected
        .iter()
        .map(|(n, null, vs)| (*n, *null, vs.map(<[&str]>::to_vec)))
        .collect();
    assert_eq!(
        actual, expected,
        "(name, nullable, values) mismatch for `{sql}`"
    );
}
