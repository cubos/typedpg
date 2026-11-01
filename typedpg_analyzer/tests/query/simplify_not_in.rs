//! `x NOT IN (SELECT y …)` → `NOT EXISTS (…)` (`Rule::NotIn`): rewritten
//! only when neither side can be NULL by what PG enforces, nothing is
//! volatile and the shape is one the rule knows. Under `pg_sanity` every
//! rewrite is also executed next to the original on the adversarial rows.

use crate::common::*;
use typedpg_analyzer::{Rule, SimplifyOptions};

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE customers (id bigint PRIMARY KEY, name text NOT NULL, region text,
                                 code int NOT NULL);
         CREATE TABLE banned (customer_id bigint NOT NULL, reason text, maybe_id bigint,
                              code int NOT NULL);
         CREATE TABLE pairs (a bigint NOT NULL, b int NOT NULL);",
    )
    .unwrap();
    db
}

/// The SQL sent for `sql`, and the rules applied to it.
fn simplified(db: &PgCatalog, sql: &str) -> (String, Vec<Rule>) {
    let a = db.analyze(sql).unwrap();
    let rules = a.simplifications.iter().map(|s| s.rule).collect();
    (a.sql, rules)
}

fn assert_rewritten(db: &PgCatalog, sql: &str, expected: &str) {
    let (sent, rules) = simplified(db, sql);
    assert_eq!(sent, expected, "for {sql}");
    assert_eq!(rules, vec![Rule::NotIn], "for {sql}");
}

fn assert_kept(db: &PgCatalog, sql: &str) {
    let a = db.analyze(sql).unwrap();
    assert!(
        a.simplifications.is_empty(),
        "{sql} was rewritten to {}",
        a.sql
    );
}

// ── Positive shapes ─────────────────────────────────────────────────────────

#[test]
fn plain_subquery_is_inlined() {
    assert_rewritten(
        &setup(),
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned)",
        "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id))",
    );
}

#[test]
fn subquery_where_is_kept_parenthesized() {
    assert_rewritten(
        &setup(),
        "SELECT c.id FROM customers c \
         WHERE c.id NOT IN (SELECT b.customer_id FROM banned b WHERE b.code = 1 OR b.code = 2)",
        "SELECT c.id FROM customers c \
         WHERE (NOT EXISTS (SELECT 1 FROM banned b WHERE (b.code = 1 OR b.code = 2) AND c.id = b.customer_id))",
    );
}

#[test]
fn correlated_subquery_is_inlined() {
    assert_rewritten(
        &setup(),
        "SELECT c.id FROM customers c \
         WHERE c.id NOT IN (SELECT b.customer_id FROM banned b WHERE b.code = c.code)",
        "SELECT c.id FROM customers c \
         WHERE (NOT EXISTS (SELECT 1 FROM banned b WHERE (b.code = c.code) AND c.id = b.customer_id))",
    );
}

#[test]
fn unqualified_left_side_a_subquery_column_would_capture_is_wrapped() {
    // `id`, moved into `FROM customers`, would read the subquery's own
    // column: the subquery is wrapped instead.
    assert_rewritten(
        &setup(),
        "SELECT id FROM customers WHERE id NOT IN (SELECT id FROM customers WHERE code = 1)",
        "SELECT id FROM customers WHERE (NOT EXISTS (SELECT 1 FROM (SELECT id FROM customers WHERE code = 1) \
         AS __typedpg_s1 (__typedpg_c1) WHERE id = __typedpg_s1.__typedpg_c1))",
    );
}

#[test]
fn limit_keeps_the_subquery_whole() {
    assert_rewritten(
        &setup(),
        "SELECT c.id FROM customers c \
         WHERE c.id NOT IN (SELECT customer_id FROM banned ORDER BY customer_id LIMIT 10)",
        "SELECT c.id FROM customers c \
         WHERE (NOT EXISTS (SELECT 1 FROM (SELECT customer_id FROM banned ORDER BY customer_id LIMIT 10) \
         AS __typedpg_s1 (__typedpg_c1) WHERE c.id = __typedpg_s1.__typedpg_c1))",
    );
}

#[test]
fn distinct_and_group_by_keep_the_subquery_whole() {
    let db = setup();
    for sub in [
        "SELECT DISTINCT customer_id FROM banned",
        "SELECT customer_id FROM banned GROUP BY customer_id HAVING count(*) > 1",
    ] {
        assert_rewritten(
            &db,
            &format!("SELECT c.id FROM customers c WHERE c.id NOT IN ({sub})"),
            &format!(
                "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM ({sub}) \
                 AS __typedpg_s1 (__typedpg_c1) WHERE c.id = __typedpg_s1.__typedpg_c1))"
            ),
        );
    }
}

#[test]
fn row_wise() {
    assert_rewritten(
        &setup(),
        "SELECT c.id FROM customers c WHERE (c.id, c.code) NOT IN (SELECT a, b FROM pairs)",
        "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM pairs WHERE c.id = a AND c.code = b))",
    );
}

#[test]
fn in_the_target_list() {
    assert_rewritten(
        &setup(),
        "SELECT c.id NOT IN (SELECT customer_id FROM banned) AS ok FROM customers c",
        "SELECT (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id)) AS ok FROM customers c",
    );
}

#[test]
fn not_before_the_left_side() {
    assert_rewritten(
        &setup(),
        "SELECT c.id FROM customers c WHERE NOT c.id IN (SELECT customer_id FROM banned)",
        "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id))",
    );
}

#[test]
fn in_delete_and_update() {
    let db = setup();
    assert_rewritten(
        &db,
        "DELETE FROM customers WHERE id NOT IN (SELECT b.customer_id FROM banned b)",
        "DELETE FROM customers WHERE (NOT EXISTS (SELECT 1 FROM banned b WHERE id = b.customer_id))",
    );
    assert_rewritten(
        &db,
        "UPDATE customers c SET name = 'x' WHERE c.id NOT IN (SELECT customer_id FROM banned)",
        "UPDATE customers c SET name = 'x' WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id))",
    );
}

#[test]
fn inner_join_columns_are_not_null() {
    assert_rewritten(
        &setup(),
        "SELECT c.id FROM customers c JOIN pairs p ON p.a = c.id \
         WHERE p.b NOT IN (SELECT code FROM banned)",
        "SELECT c.id FROM customers c JOIN pairs p ON p.a = c.id \
         WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE p.b = code))",
    );
}

#[test]
fn with_parameters() {
    let db = setup();
    let a = db
        .analyze(
            "SELECT c.id FROM customers c WHERE c.code = $code \
             AND c.id NOT IN (SELECT customer_id FROM banned WHERE code = $code)",
        )
        .unwrap();
    assert_eq!(
        a.sql,
        "SELECT c.id FROM customers c WHERE c.code = $1 \
         AND (NOT EXISTS (SELECT 1 FROM banned WHERE (code = $1) AND c.id = customer_id))"
    );
    // The placeholders' offsets follow the rewrite.
    let offsets: Vec<&str> = a.params[0]
        .sql_offsets
        .iter()
        .map(|&o| &a.sql[o - 2..o])
        .collect();
    assert_eq!(offsets, vec!["$1", "$1"]);
    assert_eq!(
        a.simplifications[0].original_sql,
        "SELECT c.id FROM customers c WHERE c.code = $1 \
             AND c.id NOT IN (SELECT customer_id FROM banned WHERE code = $1)"
    );
}

#[test]
fn every_not_in_of_the_query() {
    let db = setup();
    let (sent, rules) = simplified(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned) \
         AND c.code NOT IN (SELECT b FROM pairs)",
    );
    assert_eq!(
        sent,
        "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id)) \
         AND (NOT EXISTS (SELECT 1 FROM pairs WHERE c.code = b))"
    );
    assert_eq!(rules, vec![Rule::NotIn, Rule::NotIn]);
}

// ── Negatives: one per condition ────────────────────────────────────────────

#[test]
fn nullable_left_side_is_kept() {
    assert_kept(
        &setup(),
        "SELECT c.id FROM customers c WHERE c.region NOT IN (SELECT reason FROM banned WHERE reason IS NOT NULL)",
    );
}

#[test]
fn nullable_subquery_column_is_kept() {
    // `maybe_id IS NOT NULL` makes the analyzer infer it non-NULL, but the
    // rule only trusts the column's own NOT NULL.
    assert_kept(
        &setup(),
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT maybe_id FROM banned WHERE maybe_id IS NOT NULL)",
    );
}

#[test]
fn outer_join_nullable_side_is_kept() {
    let db = setup();
    assert_kept(
        &db,
        "SELECT c.id FROM customers c LEFT JOIN pairs p ON p.a = c.id \
         WHERE p.b NOT IN (SELECT code FROM banned)",
    );
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN \
         (SELECT b.customer_id FROM pairs p LEFT JOIN banned b ON b.code = p.b)",
    );
}

#[test]
fn check_constraint_is_not_trusted() {
    let mut db = setup();
    db.apply_sql("CREATE TABLE checked (v bigint CHECK (v IS NOT NULL));")
        .unwrap();
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT v FROM checked)",
    );
}

#[test]
fn not_valid_not_null_is_not_trusted() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE late (v bigint);
         ALTER TABLE late ADD CONSTRAINT v_nn NOT NULL v NOT VALID;",
    )
    .unwrap();
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT v FROM late)",
    );
}

#[test]
fn not_null_domain_is_not_trusted() {
    let mut db = setup();
    db.apply_sql(
        "CREATE DOMAIN nn_id AS bigint NOT NULL;
         CREATE TABLE dom (v nn_id);",
    )
    .unwrap();
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT v FROM dom)",
    );
}

#[test]
fn no_inherit_not_null_on_a_parent_is_not_trusted() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE parent (v bigint);
         ALTER TABLE parent ADD CONSTRAINT v_nn NOT NULL v NO INHERIT;
         CREATE TABLE child () INHERITS (parent);",
    )
    .unwrap();
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT v FROM parent)",
    );
}

#[test]
fn grouping_sets_are_kept() {
    assert_kept(
        &setup(),
        "SELECT c.code NOT IN (SELECT code FROM banned) FROM customers c GROUP BY ROLLUP (c.code)",
    );
}

#[test]
fn expression_sides_are_kept() {
    let db = setup();
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id + 1 NOT IN (SELECT customer_id FROM banned)",
    );
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id + 1 FROM banned)",
    );
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT 1 FROM banned)",
    );
}

#[test]
fn volatile_subquery_is_kept() {
    assert_kept(
        &setup(),
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned WHERE random() < 0.5)",
    );
}

#[test]
fn row_security_is_kept() {
    let mut db = setup();
    db.apply_sql("ALTER TABLE banned ENABLE ROW LEVEL SECURITY;")
        .unwrap();
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned)",
    );
}

#[test]
fn other_operators_and_forms_are_kept() {
    let db = setup();
    // An explicit operator: not the `IN` shape.
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE NOT (c.id = ANY (SELECT customer_id FROM banned))",
    );
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id <> ALL (SELECT customer_id FROM banned)",
    );
    // Plain IN needs no rewrite.
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id IN (SELECT customer_id FROM banned)",
    );
}

#[test]
fn set_operation_subquery_is_kept() {
    assert_kept(
        &setup(),
        "SELECT c.id FROM customers c WHERE c.id NOT IN \
         (SELECT customer_id FROM banned UNION SELECT a FROM pairs)",
    );
}

#[test]
fn subquery_from_a_view_or_cte_is_kept() {
    let mut db = setup();
    db.apply_sql("CREATE VIEW banned_v AS SELECT customer_id FROM banned;")
        .unwrap();
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned_v)",
    );
    assert_kept(
        &db,
        "WITH banned AS (SELECT maybe_id AS customer_id FROM banned) \
         SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned)",
    );
}

#[test]
fn left_side_from_a_subquery_is_kept() {
    assert_kept(
        &setup(),
        "SELECT s.id FROM (SELECT id FROM customers) s WHERE s.id NOT IN (SELECT customer_id FROM banned)",
    );
}

#[test]
fn matched_group_by_expression_is_kept() {
    assert_kept(
        &setup(),
        "SELECT c.id NOT IN (SELECT customer_id FROM banned) FROM customers c \
         GROUP BY c.id NOT IN (SELECT customer_id FROM banned)",
    );
}

#[test]
fn locking_subquery_is_kept() {
    assert_kept(
        &setup(),
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned FOR UPDATE)",
    );
}

#[test]
fn parenthesized_left_side_is_kept() {
    assert_kept(
        &setup(),
        "SELECT c.id FROM customers c WHERE (c.id) NOT IN (SELECT customer_id FROM banned)",
    );
}

#[test]
fn reserved_prefix_in_the_query_is_kept() {
    assert_kept(
        &setup(),
        "SELECT c.id AS __typedpg_x FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned)",
    );
}

#[test]
fn spreads_are_skipped() {
    assert_kept(
        &setup(),
        "SELECT c.id FROM customers c WHERE c.code IN $..codes \
         AND c.id NOT IN (SELECT customer_id FROM banned)",
    );
}

#[test]
fn foreign_tables_in_a_tree_are_not_trusted() {
    // PG enforces no NOT NULL on a foreign table, which can be a partition
    // or an inheritance child of a table that has one.
    let mut db = setup();
    db.apply_sql(
        "CREATE FOREIGN DATA WRAPPER dummy;
         CREATE SERVER nowhere FOREIGN DATA WRAPPER dummy;
         CREATE TABLE pt (id int NOT NULL, n int NOT NULL) PARTITION BY LIST (id);
         CREATE FOREIGN TABLE pt1 PARTITION OF pt FOR VALUES IN (1) SERVER nowhere;
         CREATE TABLE ip (n int NOT NULL);
         CREATE FOREIGN TABLE ic (n int NOT NULL) INHERITS (ip) SERVER nowhere;
         CREATE FOREIGN TABLE f (n int NOT NULL) SERVER nowhere;",
    )
    .unwrap();
    db.skip_pg_sanity();
    for table in ["pt", "ip", "f"] {
        // On either side.
        assert_kept(
            &db,
            &format!("SELECT c.code FROM customers c WHERE c.code NOT IN (SELECT n FROM {table})"),
        );
        assert_kept(
            &db,
            &format!("SELECT x.n FROM {table} x WHERE x.n NOT IN (SELECT code FROM banned)"),
        );
    }
}

#[test]
fn an_alias_column_list_renames_by_position() {
    // `x(b, a)`: `x.a` is the nullable `t.b`.
    let mut db = setup();
    db.apply_sql("CREATE TABLE t (a int NOT NULL, b int);")
        .unwrap();
    assert_kept(
        &db,
        "SELECT c.code FROM customers c WHERE c.code NOT IN (SELECT x.a FROM t AS x(b, a))",
    );
    assert_kept(
        &db,
        "SELECT x.a FROM t AS x(b, a) WHERE x.a NOT IN (SELECT code FROM banned)",
    );
    // A partial list too: `y(m)` renames `t.a` to `m`, `y.b` stays `t.b`.
    assert_kept(
        &db,
        "SELECT c.code FROM customers c WHERE c.code NOT IN (SELECT y.m FROM t AS y(m))",
    );
}

#[test]
fn an_unaliased_function_in_from_binds_its_name() {
    // `unnest(…)` without an alias is named `unnest`: in the subquery,
    // `unnest.unnest` is the function's NULL, not the outer column.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE u (unnest int NOT NULL);
         CREATE TABLE b (id int NOT NULL);",
    )
    .unwrap();
    assert_kept(
        &db,
        "SELECT unnest.unnest FROM u AS unnest WHERE unnest.unnest NOT IN \
         (SELECT unnest.unnest FROM b, unnest(ARRAY[NULL::int]))",
    );
    // An unaliased subquery or VALUES too (named by PG's own rules).
    assert_kept(
        &db,
        "SELECT x.id FROM b AS x WHERE x.id NOT IN \
         (SELECT x.id FROM u, (SELECT NULL::int AS id))",
    );
}

#[test]
fn a_user_equality_operator_is_not_trusted() {
    // A user `=` may return NULL for non-NULL operands.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN d AS int;
         CREATE FUNCTION d_eq(d, d) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT NULL::bool';
         CREATE OPERATOR = (LEFTARG = d, RIGHTARG = d, FUNCTION = d_eq);
         CREATE TABLE l (v d NOT NULL);
         CREATE TABLE r (v d NOT NULL);
         CREATE DOMAIN d2 AS int;
         CREATE TABLE l2 (v d2 NOT NULL);
         CREATE TABLE r2 (v d2 NOT NULL);",
    )
    .unwrap();
    assert_kept(
        &db,
        "SELECT l.v FROM l WHERE l.v NOT IN (SELECT r.v FROM r)",
    );
    // A domain without an `=` of its own compares with its base type's.
    assert_rewritten(
        &db,
        "SELECT l2.v FROM l2 WHERE l2.v NOT IN (SELECT r2.v FROM r2)",
        "SELECT l2.v FROM l2 WHERE (NOT EXISTS (SELECT 1 FROM r2 WHERE l2.v = r2.v))",
    );
}

#[test]
fn a_volatile_implicit_cast_is_not_rewritten_around() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TYPE ct AS (x int);
         CREATE FUNCTION vcast(ct) RETURNS int LANGUAGE sql VOLATILE AS 'SELECT 1';
         CREATE CAST (ct AS int) WITH FUNCTION vcast(ct) AS IMPLICIT;",
    )
    .unwrap();
    assert_kept(
        &db,
        "SELECT c.code FROM customers c WHERE c.code NOT IN (SELECT code FROM banned)",
    );
}

#[test]
fn a_volatile_domain_check_is_not_rewritten_around() {
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION vcheck(int) RETURNS bool LANGUAGE sql VOLATILE AS 'SELECT true';
         CREATE DOMAIN checked_int AS int CHECK (vcheck(VALUE));",
    )
    .unwrap();
    assert_kept(
        &db,
        "SELECT c.code FROM customers c WHERE c.code NOT IN (SELECT code FROM banned)",
    );
}

#[test]
fn a_line_comment_at_the_end_of_the_subquery_stays_closed() {
    let db = setup();
    assert_rewritten(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned -- note\n)",
        "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id))",
    );
    assert_rewritten(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN \
         (SELECT customer_id FROM banned LIMIT 5 -- note\n)",
        "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM (SELECT customer_id FROM \
         banned LIMIT 5) AS __typedpg_s1 (__typedpg_c1) WHERE c.id = __typedpg_s1.__typedpg_c1))",
    );
}

#[test]
fn two_wrapped_sites_get_their_own_aliases() {
    let (sent, rules) = simplified(
        &setup(),
        "SELECT c.id FROM customers c \
         WHERE c.id NOT IN (SELECT customer_id FROM banned LIMIT 5) \
         AND c.code NOT IN (SELECT DISTINCT code FROM banned)",
    );
    assert_eq!(
        sent,
        "SELECT c.id FROM customers c \
         WHERE (NOT EXISTS (SELECT 1 FROM (SELECT customer_id FROM banned LIMIT 5) AS __typedpg_s1 \
         (__typedpg_c1) WHERE c.id = __typedpg_s1.__typedpg_c1)) \
         AND (NOT EXISTS (SELECT 1 FROM (SELECT DISTINCT code FROM banned) AS __typedpg_s2 \
         (__typedpg_c1) WHERE c.code = __typedpg_s2.__typedpg_c1))"
    );
    assert_eq!(rules, vec![Rule::NotIn, Rule::NotIn]);
}

#[test]
fn nested_wrapped_sites_are_rewritten_in_turn() {
    // The inner site is inside the outer's subquery: a second pass
    // rewrites it, with a new alias.
    let (sent, rules) = simplified(
        &setup(),
        "SELECT c.id FROM customers c WHERE c.id NOT IN \
         (SELECT b.customer_id FROM banned b WHERE b.code NOT IN \
         (SELECT p.b FROM pairs p LIMIT 3) LIMIT 7)",
    );
    assert_eq!(rules, vec![Rule::NotIn, Rule::NotIn], "{sent}");
    assert!(
        sent.contains("__typedpg_s1") && sent.contains("__typedpg_s2"),
        "{sent}"
    );
}

#[cfg(feature = "pg_sanity")]
#[test]
fn the_oracle_compares_dml_table_contents() {
    // A wrong rewrite of an UPDATE's SET that affects the same rows: only
    // the table's contents tell it apart.
    let db = setup();
    let written = "UPDATE customers c SET region = \
                   (c.id NOT IN (SELECT customer_id FROM banned))::text";
    let wrong = "UPDATE customers c SET region = \
                 (c.id IN (SELECT customer_id FROM banned))::text";
    let divergence = db
        .pg_sanity_check_rewrite(written, wrong)
        .expect("the oracle sees the difference");
    assert_eq!(
        divergence.kind,
        typedpg_analyzer::DivergenceKind::Simplification
    );
    assert!(
        divergence.message.contains("table contents"),
        "{divergence}"
    );
    // The right one passes.
    assert!(
        db.pg_sanity_check_rewrite(
            written,
            "UPDATE customers c SET region = \
             (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id))::text",
        )
        .is_none()
    );
}

// ── Opt-outs ────────────────────────────────────────────────────────────────

#[test]
fn opt_out_globally_and_per_rule() {
    let sql = "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned)";
    let mut db = setup();
    db.set_simplify_options(SimplifyOptions {
        enabled: false,
        ..Default::default()
    });
    assert_kept(&db, sql);
    db.set_simplify_options(SimplifyOptions {
        enabled: true,
        disabled: [Rule::NotIn].into_iter().collect(),
    });
    assert_kept(&db, sql);
    db.set_simplify_options(SimplifyOptions::default());
    assert_eq!(simplified(&db, sql).1, vec![Rule::NotIn]);
}

#[test]
fn inherited_not_null_and_partitions_are_trusted() {
    // The counterpart of the negatives above: the same shapes, with the
    // NOT NULL every child carries, are rewritten.
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE parent (v bigint NOT NULL);
         CREATE TABLE child () INHERITS (parent);
         CREATE TABLE part (v bigint NOT NULL) PARTITION BY RANGE (v);
         CREATE TABLE part_1 PARTITION OF part FOR VALUES FROM (0) TO (10);",
    )
    .unwrap();
    for table in ["parent", "part"] {
        assert_rewritten(
            &db,
            &format!("SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT v FROM {table})"),
            &format!(
                "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM {table} WHERE c.id = v))"
            ),
        );
    }
}
