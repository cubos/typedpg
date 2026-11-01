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
         AS __typedpg_s1 (__typedpg_c1) WHERE customers.id = __typedpg_s1.__typedpg_c1))",
    );
}

#[test]
fn limit_or_offset_is_kept() {
    // The written SubPlan reads the subquery once; an anti-join may rescan
    // it per outer row, and a LIMIT (ordered or not: ties) may return other
    // rows on a rescan.
    let db = setup();
    for sub in [
        "SELECT customer_id FROM banned LIMIT 10",
        "SELECT customer_id FROM banned ORDER BY customer_id LIMIT 10",
        "SELECT customer_id FROM banned OFFSET 1",
        "SELECT customer_id FROM banned ORDER BY customer_id FETCH FIRST 1 ROWS WITH TIES",
    ] {
        assert_kept(
            &db,
            &format!("SELECT c.id FROM customers c WHERE c.id NOT IN ({sub})"),
        );
    }
    // An ORDER BY alone keeps the subquery whole.
    assert_rewritten(
        &db,
        "SELECT c.id FROM customers c \
         WHERE c.id NOT IN (SELECT customer_id FROM banned ORDER BY customer_id)",
        "SELECT c.id FROM customers c \
         WHERE (NOT EXISTS (SELECT 1 FROM (SELECT customer_id FROM banned ORDER BY customer_id) \
         AS __typedpg_s1 (__typedpg_c1) WHERE c.id = __typedpg_s1.__typedpg_c1))",
    );
}

#[test]
fn distinct_and_group_by_keep_the_subquery_whole() {
    let db = setup();
    for sub in [
        "SELECT DISTINCT customer_id FROM banned",
        "SELECT customer_id FROM banned GROUP BY customer_id HAVING customer_id > 1",
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
        "DELETE FROM customers WHERE (NOT EXISTS (SELECT 1 FROM banned b WHERE customers.id = b.customer_id))",
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
    // One in the target list, one in the WHERE (two NOT IN conjuncts of
    // one WHERE are each other's sibling sublinks: both stay).
    let (sent, rules) = simplified(
        &db,
        "SELECT c.code NOT IN (SELECT b FROM pairs) AS ok FROM customers c \
         WHERE c.id NOT IN (SELECT customer_id FROM banned)",
    );
    assert_eq!(
        sent,
        "SELECT (NOT EXISTS (SELECT 1 FROM pairs WHERE c.code = b)) AS ok FROM customers c \
         WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id))"
    );
    assert_eq!(rules, vec![Rule::NotIn, Rule::NotIn]);
    assert_kept(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned) \
         AND c.code NOT IN (SELECT b FROM pairs)",
    );
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
         (SELECT DISTINCT customer_id FROM banned -- note\n)",
        "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM (SELECT DISTINCT customer_id FROM \
         banned) AS __typedpg_s1 (__typedpg_c1) WHERE c.id = __typedpg_s1.__typedpg_c1))",
    );
}

#[test]
fn two_wrapped_sites_get_their_own_aliases() {
    let (sent, rules) = simplified(
        &setup(),
        "SELECT c.code NOT IN (SELECT DISTINCT code FROM banned) AS ok FROM customers c \
         WHERE c.id NOT IN (SELECT DISTINCT customer_id FROM banned)",
    );
    assert_eq!(
        sent,
        "SELECT (NOT EXISTS (SELECT 1 FROM (SELECT DISTINCT code FROM banned) AS __typedpg_s2 \
         (__typedpg_c1) WHERE c.code = __typedpg_s2.__typedpg_c1)) AS ok FROM customers c \
         WHERE (NOT EXISTS (SELECT 1 FROM (SELECT DISTINCT customer_id FROM banned) AS __typedpg_s1 \
         (__typedpg_c1) WHERE c.id = __typedpg_s1.__typedpg_c1))"
    );
    assert_eq!(rules, vec![Rule::NotIn, Rule::NotIn]);
}

#[test]
fn a_site_whose_subquery_holds_another_is_kept() {
    // The inner site is nested; the outer one's subquery holds a sublink.
    assert_kept(
        &setup(),
        "SELECT c.id FROM customers c WHERE c.id NOT IN \
         (SELECT DISTINCT b.customer_id FROM banned b WHERE b.code NOT IN \
         (SELECT DISTINCT p.b FROM pairs p))",
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

#[test]
fn a_user_implicit_cast_before_the_equality_is_not_trusted() {
    // `int = text` resolves to the built-in `int = int` through the user's
    // implicit cast, which may return NULL.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION tx2int(text) RETURNS int LANGUAGE sql IMMUTABLE \
           AS $$SELECT CASE WHEN $1 = 'n' THEN NULL ELSE length($1) END$$;
         CREATE CAST (text AS int) WITH FUNCTION tx2int(text) AS IMPLICIT;
         CREATE TABLE t (a int NOT NULL, s text NOT NULL);
         CREATE TABLE u (a int NOT NULL, s text NOT NULL);",
    )
    .unwrap();
    assert_kept(&db, "SELECT t.a FROM t WHERE t.a NOT IN (SELECT s FROM u)");
    assert_kept(
        &db,
        "SELECT t.a FROM t WHERE (t.a, t.s) NOT IN (SELECT u.a, u.a FROM u)",
    );
    // Equal types need no cast.
    assert_rewritten(
        &db,
        "SELECT t.a FROM t WHERE t.a NOT IN (SELECT u.a FROM u)",
        "SELECT t.a FROM t WHERE (NOT EXISTS (SELECT 1 FROM u WHERE t.a = u.a))",
    );
}

#[test]
fn a_cross_type_equality_needing_a_cast_is_kept() {
    // `int8 = int4` has an operator of its own; `numeric = int4` casts.
    let mut db = setup();
    db.apply_sql("CREATE TABLE amounts (v numeric NOT NULL);")
        .unwrap();
    assert_kept(
        &db,
        "SELECT c.code FROM customers c WHERE c.code NOT IN (SELECT v FROM amounts)",
    );
    assert_rewritten(
        &db,
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT code FROM banned)",
        "SELECT c.id FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = code))",
    );
}

#[test]
fn a_subquery_expression_that_can_fail_is_kept() {
    // The anti-join filters `u` by `1/u.b > 0` before looking at `t`: on
    // a row the SubPlan never reached, it divides by zero.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (k int NOT NULL, a int NOT NULL);
         CREATE TABLE u (k int NOT NULL, a int NOT NULL, b int NOT NULL, status text NOT NULL);",
    )
    .unwrap();
    for sub in [
        "SELECT a FROM u WHERE u.k = t.k AND 1/u.b > 0",
        "SELECT a FROM u WHERE u.status LIKE 'x%'",
        "SELECT a FROM u WHERE u.status::int > 0",
        "SELECT a FROM u WHERE length(u.status) > 0",
        "SELECT a FROM u ORDER BY 1/u.b LIMIT 1",
        "SELECT a FROM u GROUP BY a HAVING count(*) > 1",
        "SELECT a FROM u LIMIT $n",
        "SELECT a FROM u, generate_series(1, 2)",
        "SELECT u.a FROM u JOIN t AS t2 USING (k)",
    ] {
        assert_kept(&db, &format!("SELECT t.a FROM t WHERE t.a NOT IN ({sub})"));
    }
    // Comparisons of columns and constants can't fail.
    assert_rewritten(
        &db,
        "SELECT t.a FROM t WHERE t.a NOT IN (SELECT a FROM u WHERE u.status = 'x' AND u.k = t.k)",
        "SELECT t.a FROM t WHERE (NOT EXISTS (SELECT 1 FROM u WHERE (u.status = 'x' AND u.k = t.k) \
         AND t.a = a))",
    );
    assert_rewritten(
        &db,
        "SELECT t.a FROM t WHERE t.a NOT IN \
         (SELECT u.a FROM u JOIN t AS t2 ON t2.k = u.k WHERE u.b IS NOT NULL)",
        "SELECT t.a FROM t WHERE (NOT EXISTS (SELECT 1 FROM u JOIN t AS t2 ON t2.k = u.k WHERE \
         (u.b IS NOT NULL) AND t.a = u.a))",
    );
}

#[test]
fn a_unicode_escaped_reserved_name_is_kept() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (a int NOT NULL, \"__typedpg_c1\" int NOT NULL);
         CREATE TABLE u (a int NOT NULL);",
    )
    .unwrap();
    assert_kept(
        &db,
        "SELECT t.a FROM t WHERE U&\"\\005F\\005Ftypedpg_c1\" NOT IN (SELECT DISTINCT a FROM u)",
    );
}

#[test]
fn the_left_side_is_qualified_where_it_moves() {
    // Next to the wrapper the left-hand column is written with its
    // table's name, so nothing there can capture it.
    assert_rewritten(
        &setup(),
        "SELECT code FROM customers WHERE code NOT IN (SELECT DISTINCT code FROM banned)",
        "SELECT code FROM customers WHERE (NOT EXISTS (SELECT 1 FROM (SELECT DISTINCT code FROM banned) \
         AS __typedpg_s1 (__typedpg_c1) WHERE customers.code = __typedpg_s1.__typedpg_c1))",
    );
}

#[test]
fn the_pair_must_share_a_collation() {
    // An anti-join hashes both sides: two collations can't be hashed
    // together, where `x = ANY (sub)` merged them.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE r1 (a text COLLATE \"C\" NOT NULL, d text NOT NULL);
         CREATE TABLE r2 (b text COLLATE \"POSIX\" NOT NULL, c text COLLATE \"C\" NOT NULL,
                          d text NOT NULL);",
    )
    .unwrap();
    assert_kept(&db, "SELECT a FROM r1 WHERE a NOT IN (SELECT b FROM r2)");
    assert_kept(
        &db,
        "SELECT a FROM r1 WHERE (a, d) NOT IN (SELECT c, b FROM r2)",
    );
    // Default collation on both sides, or "C" on both.
    assert_rewritten(
        &db,
        "SELECT a FROM r1 WHERE r1.d NOT IN (SELECT r2.d FROM r2)",
        "SELECT a FROM r1 WHERE (NOT EXISTS (SELECT 1 FROM r2 WHERE r1.d = r2.d))",
    );
    assert_rewritten(
        &db,
        "SELECT a FROM r1 WHERE (a, d) NOT IN (SELECT c, d FROM r2)",
        "SELECT a FROM r1 WHERE (NOT EXISTS (SELECT 1 FROM r2 WHERE r1.a = c AND r1.d = d))",
    );
}

#[test]
fn a_constant_compared_under_a_conflicting_collation_is_kept() {
    // `'ab'` against a `name` column becomes a `name`, collation "C",
    // implicit like the column's `nd`: comparing them is a conflict on
    // every row evaluated.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE COLLATION nd (provider = icu, locale = 'und', deterministic = false);
         CREATE TABLE o (k int NOT NULL, g int NOT NULL);
         CREATE TABLE s (k int NOT NULL, g int NOT NULL, nm name COLLATE nd NOT NULL,
                         tx text COLLATE nd NOT NULL, c name NOT NULL);",
    )
    .unwrap();
    assert_kept(
        &db,
        "SELECT o.k FROM o WHERE o.k NOT IN (SELECT s.k FROM s WHERE s.g = o.g AND s.nm = 'ab')",
    );
    // A text constant has the default collation, which the column's
    // overrides; a `name` column of collation "C" agrees with the constant.
    for cond in ["s.tx = 'ab'", "s.c = 'ab'"] {
        assert_rewritten(
            &db,
            &format!("SELECT o.k FROM o WHERE o.k NOT IN (SELECT s.k FROM s WHERE {cond})"),
            &format!(
                "SELECT o.k FROM o WHERE (NOT EXISTS (SELECT 1 FROM s WHERE ({cond}) AND o.k = s.k))"
            ),
        );
    }
}

#[test]
fn an_on_condition_resolves_in_its_own_join() {
    // In `a JOIN (b JOIN c ON v = 1) ON …`, `v` isn't `a.v`: the inner ON
    // sees `b` and `c` only, so `v` is the outer `t.v` there.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (k int NOT NULL, v text NOT NULL);
         CREATE TABLE a (k int NOT NULL, v int NOT NULL);
         CREATE TABLE b (k int NOT NULL);
         CREATE TABLE c (k int NOT NULL);",
    )
    .unwrap();
    // `t.v = 1` would cast the literal to text; as `a.v` it's an int
    // comparison. Resolved in its own scope, it is the text one, which is
    // still fine; the point is that `a`'s columns aren't seen there.
    let (_, rules) = simplified(
        &db,
        "SELECT t.k FROM t WHERE t.k NOT IN \
         (SELECT a.k FROM a JOIN (b JOIN c ON b.k = c.k AND v = 'x') ON a.k = b.k)",
    );
    assert_eq!(rules, vec![Rule::NotIn]);
}

#[test]
fn an_expensive_sibling_conjunct_keeps_the_site() {
    // The NOT IN SubPlan runs before a costlier sibling and can veto the
    // row; as an anti-join it leaves the qual list and the sibling runs on
    // every row.
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION pricey(bigint) RETURNS bool LANGUAGE sql IMMUTABLE COST 100000 \
           AS 'SELECT $1 > 0';",
    )
    .unwrap();
    for sibling in [
        "(SELECT 1 / min(p.b) FROM pairs p WHERE p.a = c.id) > 0",
        "c.code IN (SELECT b FROM pairs)",
        "pricey(c.id)",
    ] {
        assert_kept(
            &db,
            &format!(
                "SELECT c.id FROM customers c \
                 WHERE c.id NOT IN (SELECT customer_id FROM banned) AND {sibling}"
            ),
        );
    }
    // In HAVING too.
    assert_kept(
        &db,
        "SELECT c.code FROM customers c GROUP BY c.code \
         HAVING c.code NOT IN (SELECT code FROM banned) AND pricey(c.code)",
    );
}

#[test]
fn a_sibling_that_can_fail_keeps_the_site() {
    // A hashed SubPlan costs as little as `int4div`: the written order
    // wins the tie and the site vetoes `a = 0` before `10 / a` runs; the
    // anti-join leaves `10 / a > 1` a filter on every row.
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE t (a int NOT NULL, s text NOT NULL);
         CREATE TABLE u (b int NOT NULL);",
    )
    .unwrap();
    for cond in [
        "a NOT IN (SELECT b FROM u) AND 10 / a > 1",
        "a NOT IN (SELECT b FROM u) AND s::int > 0",
        "(a NOT IN (SELECT b FROM u) OR a = 1) AND 10 / a > 1",
        "NOT (a NOT IN (SELECT b FROM u) AND 10 / a > 1)",
        "a NOT IN (SELECT b FROM u) AND s LIKE 'x%'",
    ] {
        assert_kept(&db, &format!("SELECT t.a FROM t WHERE {cond}"));
        assert_kept(&db, &format!("DELETE FROM t WHERE {cond}"));
        assert_kept(&db, &format!("UPDATE t SET s = 'y' WHERE {cond}"));
    }
    assert_kept(
        &db,
        "SELECT t.a FROM t GROUP BY t.a HAVING t.a NOT IN (SELECT b FROM u) AND 10 / t.a > 1",
    );
    // Without the sibling, each form is rewritten.
    for sql in [
        "SELECT t.a FROM t WHERE a NOT IN (SELECT b FROM u)",
        "DELETE FROM t WHERE a NOT IN (SELECT b FROM u)",
        "UPDATE t SET s = 'y' WHERE a NOT IN (SELECT b FROM u)",
    ] {
        assert_eq!(simplified(&db, sql).1, vec![Rule::NotIn], "{sql}");
    }
    // Not every harmless sibling is provable: `now()` is a function call.
    assert_kept(
        &db,
        "SELECT t.a FROM t WHERE a NOT IN (SELECT b FROM u) AND now() > '2000-01-01'",
    );
}

#[test]
fn error_free_siblings_are_fine() {
    let db = setup();
    assert_rewritten(
        &db,
        "SELECT c.id FROM customers c \
         WHERE c.region = 'x' AND c.id NOT IN (SELECT customer_id FROM banned) \
         AND c.id > 5 AND c.name IS NOT NULL",
        "SELECT c.id FROM customers c \
         WHERE c.region = 'x' AND (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id)) \
         AND c.id > 5 AND c.name IS NOT NULL",
    );
    assert_rewritten(
        &db,
        "SELECT c.id FROM customers c \
         WHERE (c.id NOT IN (SELECT customer_id FROM banned) OR c.code = 1) AND NOT c.id < 0",
        "SELECT c.id FROM customers c \
         WHERE ((NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id)) OR c.code = 1) AND NOT c.id < 0",
    );
    assert_rewritten(
        &db,
        "DELETE FROM customers c WHERE c.id NOT IN (SELECT customer_id FROM banned) AND c.code = 3",
        "DELETE FROM customers c WHERE (NOT EXISTS (SELECT 1 FROM banned WHERE c.id = customer_id)) \
         AND c.code = 3",
    );
}

#[test]
fn only_top_level_where_and_target_list_sites() {
    // A sublink qual may veto a row on a scan below a join whose ON can
    // fail; the anti-join runs above it.
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL);
         CREATE TABLE u (z int NOT NULL, k int NOT NULL);
         CREATE TABLE sx (b int NOT NULL);",
    )
    .unwrap();
    for sql in [
        // The ON can fail.
        "SELECT t.id FROM t JOIN u ON t.id = u.k AND 10 / (u.z * t.a) > 1 \
         WHERE t.a NOT IN (SELECT b FROM sx)",
        "SELECT t.id FROM t LEFT JOIN u ON t.id = u.k AND 10 / (u.z * t.a) > 1 \
         WHERE t.a NOT IN (SELECT b FROM sx)",
        // LATERAL, a FROM function, a derived table, a CTE in FROM.
        "SELECT t.id FROM t, LATERAL (SELECT 10 / t.a AS q) l WHERE t.a NOT IN (SELECT b FROM sx)",
        "SELECT t.id FROM t, generate_series(1, 2) g WHERE t.a NOT IN (SELECT b FROM sx)",
        "SELECT d.id FROM (SELECT id, a FROM t WHERE 10 / a > 1) d \
         WHERE d.a NOT IN (SELECT b FROM sx)",
        // Nested: in a subquery, a FROM subquery, a CTE, HAVING.
        "SELECT u.k FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.a NOT IN (SELECT b FROM sx))",
        "SELECT d.id FROM (SELECT t.id FROM t WHERE t.a NOT IN (SELECT b FROM sx)) d",
        "WITH w AS (SELECT t.id FROM t WHERE t.a NOT IN (SELECT b FROM sx)) SELECT id FROM w",
        "SELECT t.a FROM t GROUP BY t.a HAVING t.a NOT IN (SELECT b FROM sx)",
        // A set operation's branch.
        "SELECT t.id FROM t WHERE t.a NOT IN (SELECT b FROM sx) UNION SELECT 1",
    ] {
        assert_kept(&db, sql);
    }
    // An ON that can't fail.
    assert_rewritten(
        &db,
        "SELECT t.id FROM t JOIN u ON t.id = u.k WHERE t.a NOT IN (SELECT b FROM sx)",
        "SELECT t.id FROM t JOIN u ON t.id = u.k WHERE (NOT EXISTS (SELECT 1 FROM sx WHERE t.a = b))",
    );
    // The target list, whatever the FROM.
    assert_rewritten(
        &db,
        "SELECT t.a NOT IN (SELECT b FROM sx) AS ok \
         FROM t JOIN u ON t.id = u.k AND 10 / (u.z * t.a) > 1, generate_series(1, 2) g",
        "SELECT (NOT EXISTS (SELECT 1 FROM sx WHERE t.a = b)) AS ok \
         FROM t JOIN u ON t.id = u.k AND 10 / (u.z * t.a) > 1, generate_series(1, 2) g",
    );
    // UPDATE / DELETE top-level WHERE, and an UPDATE's SET.
    assert_rewritten(
        &db,
        "DELETE FROM t USING u WHERE t.id = u.k AND t.a NOT IN (SELECT b FROM sx)",
        "DELETE FROM t USING u WHERE t.id = u.k AND (NOT EXISTS (SELECT 1 FROM sx WHERE t.a = b))",
    );
    assert_rewritten(
        &db,
        "UPDATE t SET a = 1 FROM u WHERE t.id = u.k AND t.a NOT IN (SELECT b FROM sx)",
        "UPDATE t SET a = 1 FROM u WHERE t.id = u.k AND (NOT EXISTS (SELECT 1 FROM sx WHERE t.a = b))",
    );
    assert_kept(
        &db,
        "UPDATE t SET a = 1 FROM (SELECT k FROM u WHERE 10 / z > 1) d \
         WHERE t.id = d.k AND t.a NOT IN (SELECT b FROM sx)",
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
