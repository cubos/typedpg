//! Rule::Distinct and Rule::UnionAll (#173): drop a `DISTINCT` and turn a
//! `UNION` into `UNION ALL` when the schema's unique keys prove the rows
//! already distinct.

use crate::common::*;
use typedpg_analyzer::{Rule, SimplifyOptions};

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE customers (
            id bigint PRIMARY KEY,
            name text NOT NULL,
            region text NOT NULL,
            email text NOT NULL UNIQUE
         );
         CREATE TABLE orders (
            id bigint PRIMARY KEY,
            customer_id bigint NOT NULL REFERENCES customers,
            total numeric NOT NULL,
            note text
         );
         CREATE TABLE profiles (customer_id bigint PRIMARY KEY REFERENCES customers, bio text);
         CREATE TABLE lines (
            order_id bigint NOT NULL REFERENCES orders,
            n int NOT NULL,
            sku text NOT NULL,
            PRIMARY KEY (order_id, n)
         );
         CREATE TABLE small (id int PRIMARY KEY, label text NOT NULL);",
    )
    .unwrap();
    db
}

/// `setup` plus `extra`.
fn setup_with(extra: &str) -> PgCatalog {
    let mut db = setup();
    db.apply_sql(extra).unwrap();
    db
}

/// The SQL sent and the rules applied.
fn simplified(db: &PgCatalog, sql: &str) -> (String, Vec<Rule>) {
    let q = db.analyze(sql).unwrap();
    (q.sql, q.simplifications.iter().map(|s| s.rule).collect())
}

#[track_caller]
fn assert_rewrites(db: &PgCatalog, sql: &str, expected: &str, rules: &[Rule]) {
    assert_eq!(simplified(db, sql), (expected.to_owned(), rules.to_vec()));
}

#[track_caller]
fn assert_kept(db: &PgCatalog, sql: &str) {
    assert_eq!(simplified(db, sql), (sql.to_owned(), vec![]));
}

// ── DISTINCT: positive shapes ───────────────────────────────────────────────

#[test]
fn distinct_primary_key_in_output() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT id, name FROM customers",
        "SELECT id, name FROM customers",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_many_to_one_join() {
    // The issue's example: each order joins at most one customer.
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT o.id, o.total, c.name FROM orders o JOIN customers c ON c.id = o.customer_id",
        "SELECT o.id, o.total, c.name FROM orders o JOIN customers c ON c.id = o.customer_id",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_join_written_the_other_way_round() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT o.id, c.name FROM customers c INNER JOIN orders o ON o.customer_id = c.id",
        "SELECT o.id, c.name FROM customers c INNER JOIN orders o ON o.customer_id = c.id",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_comma_join_with_where_equality() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT o.id, c.name FROM orders o, customers c WHERE c.id = o.customer_id AND o.total < 100",
        "SELECT o.id, c.name FROM orders o, customers c WHERE c.id = o.customer_id AND o.total < 100",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_chain_of_many_to_one_joins() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT l.order_id, l.n, c.name FROM lines l \
         JOIN orders o ON o.id = l.order_id JOIN customers c ON c.id = o.customer_id",
        "SELECT l.order_id, l.n, c.name FROM lines l \
         JOIN orders o ON o.id = l.order_id JOIN customers c ON c.id = o.customer_id",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_composite_key() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT n, sku, order_id FROM lines",
        "SELECT n, sku, order_id FROM lines",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_key_equated_to_a_parameter_or_constant() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT o.id, c.name FROM orders o, customers c WHERE c.id = $id",
        "SELECT o.id, c.name FROM orders o, customers c WHERE c.id = $1",
        &[Rule::Distinct],
    );
    assert_rewrites(
        &db,
        "SELECT DISTINCT o.id, s.label FROM orders o CROSS JOIN small s WHERE s.id = 1",
        "SELECT o.id, s.label FROM orders o CROSS JOIN small s WHERE s.id = 1",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_left_join_to_a_unique_side() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT o.id, c.name FROM orders o LEFT JOIN customers c ON c.id = o.customer_id",
        "SELECT o.id, c.name FROM orders o LEFT JOIN customers c ON c.id = o.customer_id",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_left_join_with_both_keys_in_output() {
    // A customer without orders appears once, null-extended; its orders
    // differ in o.id.
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT c.id, o.id AS order_id FROM customers c LEFT JOIN orders o ON o.customer_id = c.id",
        "SELECT c.id, o.id AS order_id FROM customers c LEFT JOIN orders o ON o.customer_id = c.id",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_right_and_full_joins_with_preserved_keys() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT o.id, c.name FROM customers c RIGHT JOIN orders o ON c.id = o.customer_id",
        "SELECT o.id, c.name FROM customers c RIGHT JOIN orders o ON c.id = o.customer_id",
        &[Rule::Distinct],
    );
    assert_rewrites(
        &db,
        "SELECT DISTINCT c.id, o.id AS oid FROM customers c FULL JOIN orders o ON c.id = o.customer_id",
        "SELECT c.id, o.id AS oid FROM customers c FULL JOIN orders o ON c.id = o.customer_id",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_using_join() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT o.id, p.bio FROM orders o JOIN profiles p USING (customer_id)",
        "SELECT o.id, p.bio FROM orders o JOIN profiles p USING (customer_id)",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_unique_constraint_index_and_nulls_not_distinct() {
    let db = setup_with(
        "CREATE TABLE codes (code text NOT NULL, other text);
         CREATE UNIQUE INDEX ON codes (code);
         CREATE TABLE nnd (k int UNIQUE NULLS NOT DISTINCT, v text);",
    );
    // A UNIQUE NOT NULL column.
    assert_rewrites(
        &db,
        "SELECT DISTINCT email FROM customers",
        "SELECT email FROM customers",
        &[Rule::Distinct],
    );
    // A non-partial unique index.
    assert_rewrites(
        &db,
        "SELECT DISTINCT code, other FROM codes",
        "SELECT code, other FROM codes",
        &[Rule::Distinct],
    );
    // NULLS NOT DISTINCT: one NULL at most.
    assert_rewrites(
        &db,
        "SELECT DISTINCT k FROM nnd",
        "SELECT k FROM nnd",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_star() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT * FROM orders",
        "SELECT * FROM orders",
        &[Rule::Distinct],
    );
    assert_rewrites(
        &db,
        "SELECT DISTINCT o.*, c.name FROM orders o JOIN customers c ON c.id = o.customer_id",
        "SELECT o.*, c.name FROM orders o JOIN customers c ON c.id = o.customer_id",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_in_subqueries_and_ctes() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT id FROM customers WHERE id IN (SELECT DISTINCT c.id FROM customers c WHERE c.region = 'x')",
        "SELECT id FROM customers WHERE id IN (SELECT c.id FROM customers c WHERE c.region = 'x')",
        &[Rule::Distinct],
    );
    assert_rewrites(
        &db,
        "WITH o AS (SELECT DISTINCT id, total FROM orders) SELECT id FROM o",
        "WITH o AS (SELECT id, total FROM orders) SELECT id FROM o",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_keeps_order_by_limit_and_comments() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT /* hi */ DISTINCT  id, name FROM customers ORDER BY name, id LIMIT 10",
        "SELECT /* hi */ id, name FROM customers ORDER BY name, id LIMIT 10",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_with_window_function() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT id, count(*) OVER () FROM customers",
        "SELECT id, count(*) OVER () FROM customers",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_rule_opt_out() {
    let mut db = setup();
    let sql = "SELECT DISTINCT id, name FROM customers";
    db.set_simplify_options(SimplifyOptions {
        enabled: true,
        disabled: [Rule::Distinct].into_iter().collect(),
    });
    assert_kept(&db, sql);
    db.set_simplify_options(SimplifyOptions {
        enabled: false,
        disabled: Default::default(),
    });
    assert_kept(&db, sql);
}

// ── DISTINCT: negative ──────────────────────────────────────────────────────

#[test]
fn distinct_kept_on_a_nullable_key_column() {
    // A plain UNIQUE allows any number of NULLs.
    let db = setup_with("CREATE TABLE t (code text UNIQUE, v int);");
    assert_kept(&db, "SELECT DISTINCT code FROM t");
}

#[test]
fn distinct_kept_on_a_deferrable_unique_constraint() {
    let db = setup_with(
        "CREATE TABLE t (code text NOT NULL UNIQUE DEFERRABLE INITIALLY IMMEDIATE);
         CREATE TABLE u (id int PRIMARY KEY DEFERRABLE INITIALLY DEFERRED);",
    );
    assert_kept(&db, "SELECT DISTINCT code FROM t");
    assert_kept(&db, "SELECT DISTINCT id FROM u");
}

#[test]
fn distinct_kept_on_a_partial_unique_index() {
    let db = setup_with(
        "CREATE TABLE t (code text NOT NULL, active bool NOT NULL);
         CREATE UNIQUE INDEX ON t (code) WHERE active;",
    );
    assert_kept(&db, "SELECT DISTINCT code FROM t");
}

#[test]
fn distinct_kept_on_an_expression_index() {
    let db = setup_with(
        "CREATE TABLE t (code text NOT NULL);
         CREATE UNIQUE INDEX ON t (lower(code));",
    );
    assert_kept(&db, "SELECT DISTINCT code FROM t");
}

#[test]
fn distinct_kept_when_the_join_multiplies_rows() {
    let db = setup();
    // A customer has many orders.
    assert_kept(
        &db,
        "SELECT DISTINCT c.id, c.name FROM customers c JOIN orders o ON o.customer_id = c.id",
    );
    // A cross join.
    assert_kept(&db, "SELECT DISTINCT c.id FROM customers c, small s");
    // Equality to a non-unique column.
    assert_kept(
        &db,
        "SELECT DISTINCT o.id FROM orders o JOIN orders o2 ON o2.customer_id = o.customer_id",
    );
    // A LEFT JOIN to the many side.
    assert_kept(
        &db,
        "SELECT DISTINCT c.id FROM customers c LEFT JOIN orders o ON o.customer_id = c.id",
    );
    // Only part of a composite key equated.
    assert_kept(
        &db,
        "SELECT DISTINCT o.id FROM orders o JOIN lines l ON l.order_id = o.id",
    );
    // An OR doesn't pin the key.
    assert_kept(
        &db,
        "SELECT DISTINCT o.id FROM orders o JOIN customers c ON c.id = o.customer_id OR c.id = 1",
    );
}

#[test]
fn distinct_kept_when_the_key_is_missing_from_the_output() {
    let db = setup();
    assert_kept(
        &db,
        "SELECT DISTINCT o.total, c.name FROM orders o JOIN customers c ON c.id = o.customer_id",
    );
    // Only part of a composite key.
    assert_kept(&db, "SELECT DISTINCT order_id, sku FROM lines");
    // An expression over the key isn't the key.
    assert_kept(&db, "SELECT DISTINCT id + 0 FROM customers");
    // The nullable side's key alone: customers without orders all give NULL.
    assert_kept(
        &db,
        "SELECT DISTINCT o.id FROM customers c LEFT JOIN orders o ON o.customer_id = c.id",
    );
    // FULL JOIN: one side's key isn't enough.
    assert_kept(
        &db,
        "SELECT DISTINCT o.id FROM customers c FULL JOIN orders o ON c.id = o.customer_id",
    );
}

#[test]
fn distinct_kept_under_a_nondeterministic_collation() {
    let db = setup_with(
        "CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE TABLE t (code text COLLATE ci NOT NULL UNIQUE);
         CREATE TABLE u (code text NOT NULL UNIQUE);",
    );
    // `'a'` and `'A'` are equal under `ci`: the index allows one of them,
    // and DISTINCT compares the same way, but this doesn't tell
    // deterministic collations apart from the others.
    assert_kept(&db, "SELECT DISTINCT code FROM t");
    // An output collation other than the key's.
    assert_kept(&db, "SELECT DISTINCT code COLLATE ci FROM u");
}

#[test]
fn distinct_kept_when_the_index_compares_otherwise() {
    // text_pattern_ops is not text's default btree family.
    let db = setup_with(
        "CREATE TABLE t (code text NOT NULL);
         CREATE UNIQUE INDEX ON t (code text_pattern_ops);",
    );
    assert_kept(&db, "SELECT DISTINCT code FROM t");
}

#[test]
fn distinct_kept_when_the_join_coerces_the_key() {
    // `c.id = o.total` compares as numeric: c.id is cast first.
    let db = setup();
    assert_kept(
        &db,
        "SELECT DISTINCT o.id FROM orders o JOIN customers c ON c.id = o.total",
    );
}

#[test]
fn distinct_kept_with_srf_aggregates_group_by_or_distinct_on() {
    let db = setup();
    assert_kept(
        &db,
        "SELECT DISTINCT id, generate_series(1, 2) FROM customers",
    );
    assert_kept(&db, "SELECT DISTINCT ON (id) id, name FROM customers");
    assert_kept(&db, "SELECT DISTINCT id FROM customers GROUP BY id");
    assert_kept(&db, "SELECT DISTINCT count(*) FROM customers");
}

#[test]
fn distinct_kept_with_volatile_functions() {
    let db = setup();
    assert_kept(&db, "SELECT DISTINCT id, random() FROM customers");
    assert_kept(
        &db,
        "SELECT DISTINCT id FROM customers WHERE random() < 0.5",
    );
}

#[test]
fn distinct_kept_over_relations_it_cannot_see_through() {
    let db = setup_with(
        "CREATE VIEW v AS SELECT id, name FROM customers;
         CREATE TABLE parent (id int PRIMARY KEY);
         CREATE TABLE child () INHERITS (parent);
         CREATE TABLE part (id int PRIMARY KEY) PARTITION BY RANGE (id);
         CREATE TABLE secured (id int PRIMARY KEY);
         ALTER TABLE secured ENABLE ROW LEVEL SECURITY;",
    );
    assert_kept(&db, "SELECT DISTINCT id FROM v");
    assert_kept(&db, "SELECT DISTINCT id FROM (SELECT id FROM customers) s");
    assert_kept(
        &db,
        "WITH c AS (SELECT id FROM customers) SELECT DISTINCT id FROM c",
    );
    // A CTE that shadows a table's name.
    assert_kept(
        &db,
        "WITH customers AS (SELECT 1::bigint AS id UNION ALL SELECT 1) SELECT DISTINCT id FROM customers",
    );
    // An inheritance parent reads its children, which the index doesn't cover.
    assert_kept(&db, "SELECT DISTINCT id FROM parent");
    assert_kept(&db, "SELECT DISTINCT id FROM part");
    assert_kept(&db, "SELECT DISTINCT id FROM secured");
}

#[test]
fn distinct_kept_with_natural_joins_and_ambiguous_using_columns() {
    let db = setup();
    assert_kept(
        &db,
        "SELECT DISTINCT o.id FROM orders o NATURAL JOIN profiles p",
    );
    // `customer_id` is the merged column: no single relation's.
    assert_kept(
        &db,
        "SELECT DISTINCT customer_id FROM orders o JOIN profiles p USING (customer_id)",
    );
}

/// `t AS x(k, id)` renames t's columns by position: `x.id` is `t.k`.
fn aliased_columns_setup() -> PgCatalog {
    setup_with(
        "CREATE TABLE t (id int PRIMARY KEY, k int, v text);
         CREATE TABLE u (id int PRIMARY KEY, tid int);",
    )
}

#[test]
fn distinct_follows_an_alias_column_list() {
    let db = aliased_columns_setup();
    // `x.id` is t.k, which repeats.
    assert_kept(&db, "SELECT DISTINCT x.id FROM t AS x(k, id)");
    assert_kept(&db, "SELECT DISTINCT id FROM t AS x(k, id)");
    // `x.k` is t.id, the key.
    assert_rewrites(
        &db,
        "SELECT DISTINCT x.k FROM t AS x(k, id)",
        "SELECT x.k FROM t AS x(k, id)",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_join_follows_an_alias_column_list() {
    let db = aliased_columns_setup();
    // Equated to t.k: each u row may match several t rows.
    assert_kept(
        &db,
        "SELECT DISTINCT u.id FROM u JOIN t AS x(k, id) ON x.id = u.id",
    );
    // Equated to t.id: at most one.
    assert_rewrites(
        &db,
        "SELECT DISTINCT u.id, x.v FROM u JOIN t AS x(k, id) ON x.k = u.tid",
        "SELECT u.id, x.v FROM u JOIN t AS x(k, id) ON x.k = u.tid",
        &[Rule::Distinct],
    );
    // USING reads the alias's names too: `tid` is t.k here, t.id there.
    assert_kept(
        &db,
        "SELECT DISTINCT u.id, x.v FROM u JOIN t AS x(q, tid) USING (tid)",
    );
    assert_rewrites(
        &db,
        "SELECT DISTINCT u.id, x.v FROM u JOIN t AS x(tid) USING (tid)",
        "SELECT u.id, x.v FROM u JOIN t AS x(tid) USING (tid)",
        &[Rule::Distinct],
    );
}

#[test]
fn distinct_follows_a_partial_alias_column_list() {
    let db = aliased_columns_setup();
    // `x(z)` renames only the first column: `k` keeps its name.
    assert_kept(&db, "SELECT DISTINCT x.k FROM t AS x(z)");
    assert_rewrites(
        &db,
        "SELECT DISTINCT x.z, x.v FROM t AS x(z)",
        "SELECT x.z, x.v FROM t AS x(z)",
        &[Rule::Distinct],
    );
}

#[test]
fn union_follows_an_alias_column_list() {
    let db = aliased_columns_setup();
    assert_kept(
        &db,
        "SELECT 'a' AS kind, x.id FROM t AS x(k, id) UNION SELECT 'b', id FROM u",
    );
    assert_rewrites(
        &db,
        "SELECT 'a' AS kind, x.k FROM t AS x(k, id) UNION SELECT 'b', id FROM u",
        "SELECT 'a' AS kind, x.k FROM t AS x(k, id) UNION ALL SELECT 'b', id FROM u",
        &[Rule::UnionAll],
    );
}

#[test]
fn distinct_kept_on_a_not_valid_not_null() {
    // PG 18 sets attnotnull for a NOT VALID NOT NULL, while NULLs may stay.
    let mut db = setup_with(
        "CREATE TABLE nv (a int, UNIQUE (a));
         ALTER TABLE nv ADD CONSTRAINT nv_a_not_null NOT NULL a NOT VALID;",
    );
    assert_kept(&db, "SELECT DISTINCT a FROM nv");
    db.apply_sql("ALTER TABLE nv VALIDATE CONSTRAINT nv_a_not_null;")
        .unwrap();
    assert_rewrites(
        &db,
        "SELECT DISTINCT a FROM nv",
        "SELECT a FROM nv",
        &[Rule::Distinct],
    );
}

// ── UNION: positive shapes ──────────────────────────────────────────────────

#[test]
fn union_tagged() {
    // The issue's example.
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT 'order' AS kind, id FROM orders UNION SELECT 'customer', id FROM customers",
        "SELECT 'order' AS kind, id FROM orders UNION ALL SELECT 'customer', id FROM customers",
        &[Rule::UnionAll],
    );
}

#[test]
fn union_three_arms_with_integer_tags() {
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT 1 AS k, id, total FROM orders WHERE total > 10 \
         UNION SELECT 2, o.id, o.total FROM orders o JOIN customers c ON c.id = o.customer_id \
         UNION ALL SELECT 3, id, 0 FROM customers",
        "SELECT 1 AS k, id, total FROM orders WHERE total > 10 \
         UNION ALL SELECT 2, o.id, o.total FROM orders o JOIN customers c ON c.id = o.customer_id \
         UNION ALL SELECT 3, id, 0 FROM customers",
        &[Rule::UnionAll],
    );
}

#[test]
fn union_keeps_parentheses_order_by_and_limit() {
    // The ORDER BY is total: under a LIMIT, ties could come out either way
    // in both forms.
    let db = setup();
    assert_rewrites(
        &db,
        "(SELECT 'o' AS kind, id FROM orders ORDER BY id LIMIT 5) UNION DISTINCT \
         (SELECT 'c', id FROM customers) ORDER BY 2, 1 LIMIT 3",
        "(SELECT 'o' AS kind, id FROM orders ORDER BY id LIMIT 5) UNION ALL \
         (SELECT 'c', id FROM customers) ORDER BY 2, 1 LIMIT 3",
        &[Rule::UnionAll],
    );
}

#[test]
fn union_of_distinct_arms() {
    // The arms' own DISTINCT goes first (keys), then the UNION.
    let db = setup();
    assert_rewrites(
        &db,
        "SELECT DISTINCT 'o' AS kind, id FROM orders UNION SELECT DISTINCT 'c', id FROM customers",
        "SELECT 'o' AS kind, id FROM orders UNION ALL SELECT 'c', id FROM customers",
        &[Rule::Distinct, Rule::Distinct, Rule::UnionAll],
    );
    // A DISTINCT no key explains still makes its arm distinct.
    assert_rewrites(
        &db,
        "SELECT DISTINCT true AS b, region FROM customers UNION SELECT DISTINCT false, region FROM customers",
        "SELECT DISTINCT true AS b, region FROM customers UNION ALL SELECT DISTINCT false, region FROM customers",
        &[Rule::UnionAll],
    );
}

// ── UNION: negative ─────────────────────────────────────────────────────────

#[test]
fn union_kept_when_two_arms_share_a_tag() {
    let db = setup();
    assert_kept(
        &db,
        "SELECT 'a' AS kind, id FROM orders UNION SELECT 'a', id FROM customers",
    );
    assert_kept(
        &db,
        "SELECT 1 AS k, id FROM orders UNION SELECT 2, id FROM customers UNION SELECT 1, id FROM small",
    );
}

#[test]
fn union_kept_without_a_tag() {
    let db = setup();
    assert_kept(&db, "SELECT id FROM orders UNION SELECT id FROM customers");
    // A tag that isn't a constant in every arm.
    assert_kept(
        &db,
        "SELECT 'a' AS kind, id FROM orders UNION SELECT name, id FROM customers",
    );
}

#[test]
fn union_kept_when_an_arm_is_not_distinct() {
    let db = setup();
    assert_kept(
        &db,
        "SELECT 'o' AS kind, customer_id FROM orders UNION SELECT 'c', id FROM customers",
    );
}

#[test]
fn union_kept_when_the_key_is_coerced() {
    // small.id is int4, the output column int8.
    let db = setup();
    assert_kept(
        &db,
        "SELECT 's' AS kind, id FROM small UNION SELECT 'c', id FROM customers",
    );
}

#[test]
fn union_kept_under_a_nondeterministic_collation() {
    let db = setup_with(
        "CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE TABLE t (code text NOT NULL UNIQUE);",
    );
    assert_kept(
        &db,
        "SELECT 'a' AS kind, code COLLATE ci FROM t UNION SELECT 'b', code FROM t",
    );
}

#[test]
fn union_kept_with_intersect_or_except() {
    let db = setup();
    assert_kept(
        &db,
        "SELECT 'o' AS kind, id FROM orders UNION SELECT 'c', id FROM customers \
         EXCEPT SELECT 'c', id FROM customers WHERE id = 1",
    );
    assert_kept(
        &db,
        "SELECT 'o' AS kind, id FROM orders UNION (SELECT 'c', id FROM customers \
         INTERSECT SELECT 'c', id FROM customers)",
    );
}

#[test]
fn union_rule_opt_out() {
    let mut db = setup();
    db.set_simplify_options(SimplifyOptions {
        enabled: true,
        disabled: [Rule::UnionAll].into_iter().collect(),
    });
    assert_kept(
        &db,
        "SELECT 'order' AS kind, id FROM orders UNION SELECT 'customer', id FROM customers",
    );
}
