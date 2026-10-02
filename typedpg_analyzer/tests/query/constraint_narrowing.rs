//! Nullability narrowed by what the schema declares: a join following a
//! foreign key always finds the referenced row, a CHECK constraint holds
//! for every row, and what is known of a row combines with both. The
//! constraints are taken to hold as declared (`NOT VALID` and deferrable
//! ones included; `NOT ENFORCED` ones say nothing). Every NOT NULL below is
//! checked against PostgreSQL 18 by the pg_sanity soundness oracle, over
//! rows that satisfy the constraints.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL, email text);
         CREATE TABLE orders (
             id int PRIMARY KEY,
             customer_id int NOT NULL REFERENCES customers,
             reviewer_id int REFERENCES customers,
             note text
         );
         CREATE TABLE pair_parent (a int, b int, label text NOT NULL, PRIMARY KEY (a, b));
         CREATE TABLE pair_child (
             id int PRIMARY KEY, a int NOT NULL, b int NOT NULL,
             FOREIGN KEY (a, b) REFERENCES pair_parent
         );
         CREATE TABLE secret (id int PRIMARY KEY, v text NOT NULL);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE TABLE uses_secret (id int PRIMARY KEY, secret_id int NOT NULL REFERENCES secret);
         CREATE TABLE lax_child (
             id int PRIMARY KEY, customer_id int NOT NULL,
             FOREIGN KEY (customer_id) REFERENCES customers NOT ENFORCED
         );
         CREATE TABLE unvalidated_child (id int PRIMARY KEY, customer_id int NOT NULL);
         ALTER TABLE unvalidated_child
             ADD FOREIGN KEY (customer_id) REFERENCES customers NOT VALID;
         CREATE TABLE deferred_child (
             id int PRIMARY KEY,
             customer_id int NOT NULL REFERENCES customers DEFERRABLE INITIALLY DEFERRED
         );
         CREATE TABLE pcust (id int PRIMARY KEY, name text NOT NULL) PARTITION BY RANGE (id);
         CREATE TABLE pcust1 PARTITION OF pcust FOR VALUES FROM (0) TO (1000000);
         CREATE TABLE porders (id int PRIMARY KEY, pcust_id int NOT NULL REFERENCES pcust);
         CREATE TABLE accounts (account_id int PRIMARY KEY, owner text NOT NULL);
         CREATE TABLE invoices (id int PRIMARY KEY, account_id int NOT NULL REFERENCES accounts);
         CREATE TABLE employees (
             id int PRIMARY KEY, name text NOT NULL, manager_id int REFERENCES employees
         );
         CREATE VIEW customers_v AS SELECT * FROM customers;

         CREATE TYPE kind AS ENUM ('a', 'b');
         CREATE TABLE things (
             id int PRIMARY KEY, kind kind NOT NULL, a_id int, b_id int,
             done boolean NOT NULL, finished_at timestamptz,
             CHECK ((kind = 'a' AND a_id IS NOT NULL AND b_id IS NULL)
                 OR (kind = 'b' AND b_id IS NOT NULL AND a_id IS NULL)),
             CHECK (NOT done OR finished_at IS NOT NULL)
         );
         CREATE TABLE contacts (
             id int PRIMARY KEY, email text, phone text,
             CHECK (email IS NOT NULL OR phone IS NOT NULL)
         );
         CREATE TABLE owners (
             id int PRIMARY KEY, user_id int, org_id int,
             CHECK (num_nonnulls(user_id, org_id) = 1)
         );
         CREATE TABLE tickets (
             id int PRIMARY KEY, status text NOT NULL, closed_at timestamptz,
             CHECK (status <> 'a' OR closed_at IS NOT NULL)
         );
         CREATE TABLE weak (id int PRIMARY KEY, x int, CHECK (x > 0));
         CREATE TABLE always (id int PRIMARY KEY, x int, CHECK (x IS NOT NULL));
         CREATE TABLE lax_check (
             id int PRIMARY KEY, x int,
             CONSTRAINT x_set CHECK (x IS NOT NULL) NOT ENFORCED
         );
         CREATE TABLE unvalidated_check (id int PRIMARY KEY, x int);
         ALTER TABLE unvalidated_check ADD CHECK (x IS NOT NULL) NOT VALID;
         CREATE TABLE inh_parent (id int, x int, CONSTRAINT x_set CHECK (x IS NOT NULL) NO INHERIT);
         CREATE TABLE inh_child () INHERITS (inh_parent);
         CREATE TABLE plain (id int PRIMARY KEY, a int, b int);",
    )
    .unwrap();
    db
}

/// Each output column's nullability (`true` = nullable).
#[track_caller]
fn assert_nullable(db: &PgCatalog, sql: &str, expected: &[(&str, bool)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, bool)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    assert_eq!(actual, expected, "nullability mismatch for `{sql}`");
}

// ── Foreign keys ─────────────────────────────────────────────────────────────

#[test]
fn a_left_join_along_a_not_null_foreign_key_always_matches() {
    let db = setup();
    for sql in [
        "SELECT c.name, c.email FROM orders o LEFT JOIN customers c ON c.id = o.customer_id",
        "SELECT c.name, c.email FROM orders o LEFT JOIN customers c ON o.customer_id = c.id",
        "SELECT c.name, c.email FROM customers c RIGHT JOIN orders o ON c.id = o.customer_id",
    ] {
        assert_nullable(&db, sql, &[("name", false), ("email", true)]);
    }
    // Several key columns, all equated.
    assert_nullable(
        &db,
        "SELECT p.label FROM pair_child c LEFT JOIN pair_parent p ON p.a = c.a AND p.b = c.b",
        &[("label", false)],
    );
    // USING, when the names agree.
    assert_nullable(
        &db,
        "SELECT a.owner, account_id FROM invoices LEFT JOIN accounts a USING (account_id)",
        &[("owner", false), ("account_id", false)],
    );
    // A partitioned parent: its rows are in its partitions.
    assert_nullable(
        &db,
        "SELECT p.name FROM porders o LEFT JOIN pcust p ON p.id = o.pcust_id",
        &[("name", false)],
    );
    // A FULL join keeps only the child side's null-extension.
    assert_nullable(
        &db,
        "SELECT o.id, c.name FROM orders o FULL JOIN customers c ON c.id = o.customer_id",
        &[("id", true), ("name", false)],
    );
}

#[test]
fn declared_foreign_keys_are_trusted_unless_not_enforced() {
    let db = setup();
    let check = |child: &str, nullable: bool| {
        assert_nullable(
            &db,
            &format!("SELECT c.name FROM {child} x LEFT JOIN customers c ON c.id = x.customer_id"),
            &[("name", nullable)],
        );
    };
    check("unvalidated_child", false);
    check("deferred_child", false);
    check("lax_child", true);
}

#[test]
fn a_nullable_key_needs_proving_non_null() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT r.name FROM orders o LEFT JOIN customers r ON r.id = o.reviewer_id",
        &[("name", true)],
    );
    assert_nullable(
        &db,
        "SELECT r.name FROM orders o LEFT JOIN customers r ON r.id = o.reviewer_id
         WHERE o.reviewer_id IS NOT NULL",
        &[("name", false)],
    );
    // A self-reference.
    assert_nullable(
        &db,
        "SELECT m.name FROM employees e LEFT JOIN employees m ON m.id = e.manager_id
         WHERE e.manager_id > 0",
        &[("name", false)],
    );
}

#[test]
fn the_child_row_itself_must_be_there() {
    let db = setup();
    // A customer without orders has a NULL `o.customer_id`.
    assert_nullable(
        &db,
        "SELECT c2.name FROM customers c0
             LEFT JOIN orders o ON o.customer_id = c0.id
             LEFT JOIN customers c2 ON c2.id = o.customer_id",
        &[("name", true)],
    );
    // Unless the query keeps only customers with orders.
    assert_nullable(
        &db,
        "SELECT c2.name FROM customers c0
             LEFT JOIN orders o ON o.customer_id = c0.id
             LEFT JOIN customers c2 ON c2.id = o.customer_id
         WHERE o.id IS NOT NULL",
        &[("name", false)],
    );
}

#[test]
fn joins_that_dont_follow_a_foreign_key_stay_outer() {
    let db = setup();
    for sql in [
        // Another condition may fail.
        "SELECT c.name FROM orders o LEFT JOIN customers c
             ON c.id = o.customer_id AND c.email = 'a'",
        "SELECT c.name FROM orders o LEFT JOIN customers c
             ON c.id = o.customer_id AND o.note = 'a'",
        // Not the key's columns.
        "SELECT c.name FROM orders o LEFT JOIN customers c ON c.id = o.id",
        "SELECT p.label FROM pair_child c LEFT JOIN pair_parent p ON p.a = c.a",
        // Row security may hide the referenced row.
        "SELECT s.v AS name FROM uses_secret u LEFT JOIN secret s ON s.id = u.secret_id",
        // The parent seen through a view, a subquery, a sample, or `ONLY`
        // over a partitioned table.
        "SELECT c.name FROM orders o LEFT JOIN customers_v c ON c.id = o.customer_id",
        "SELECT c.name FROM orders o
             LEFT JOIN (SELECT * FROM customers WHERE email IS NOT NULL) c ON c.id = o.customer_id",
        "SELECT c.name FROM orders o
             LEFT JOIN customers c TABLESAMPLE BERNOULLI (50) ON c.id = o.customer_id",
        "SELECT p.name FROM porders o LEFT JOIN ONLY pcust p ON p.id = o.pcust_id",
        // The parent side is more than the parent.
        "SELECT c.name FROM orders o
             LEFT JOIN (customers c JOIN pair_parent pp ON pp.a = c.id) ON c.id = o.customer_id",
    ] {
        let s = db.analyze(sql).unwrap();
        assert!(s.columns[0].nullable, "`{sql}` should stay nullable");
    }
    // The wrong direction: a customer may have no order.
    assert_nullable(
        &db,
        "SELECT o.note FROM customers c LEFT JOIN orders o ON o.customer_id = c.id",
        &[("note", true)],
    );
}

// ── CHECK constraints ────────────────────────────────────────────────────────

#[test]
fn a_discriminated_row_has_its_branch_s_columns() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT a_id, b_id FROM things WHERE kind = 'a'",
        &[("a_id", false), ("b_id", true)],
    );
    assert_nullable(
        &db,
        "SELECT a_id, b_id FROM things WHERE kind = 'b'",
        &[("a_id", true), ("b_id", false)],
    );
    // Without knowing the kind: one of them, so COALESCE is NOT NULL.
    assert_nullable(
        &db,
        "SELECT a_id, b_id, coalesce(a_id, b_id) AS either FROM things",
        &[("a_id", true), ("b_id", true), ("either", false)],
    );
    // CASE branches know the kind.
    assert_nullable(
        &db,
        "SELECT CASE WHEN kind = 'a' THEN a_id WHEN kind = 'b' THEN b_id ELSE 0 END AS v
         FROM things",
        &[("v", false)],
    );
    // A boolean column as the condition.
    assert_nullable(
        &db,
        "SELECT finished_at FROM things WHERE done",
        &[("finished_at", false)],
    );
    assert_nullable(
        &db,
        "SELECT finished_at FROM things WHERE NOT done",
        &[("finished_at", true)],
    );
    // A text discriminator.
    assert_nullable(
        &db,
        "SELECT closed_at FROM tickets WHERE status = 'a'",
        &[("closed_at", false)],
    );
    assert_nullable(
        &db,
        "SELECT closed_at FROM tickets WHERE status = 'b'",
        &[("closed_at", true)],
    );
}

#[test]
fn at_least_one_of_them() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT email, coalesce(email, phone) AS reach, greatest(email, phone) AS g FROM contacts",
        &[("email", true), ("reach", false), ("g", false)],
    );
    assert_nullable(
        &db,
        "SELECT phone FROM contacts WHERE email IS NULL",
        &[("phone", false)],
    );
    assert_nullable(
        &db,
        "SELECT CASE WHEN email IS NULL THEN phone ELSE email END AS reach FROM contacts",
        &[("reach", false)],
    );
    assert_nullable(
        &db,
        "SELECT coalesce(user_id, org_id) AS owner FROM owners",
        &[("owner", false)],
    );
    assert_nullable(
        &db,
        "SELECT org_id FROM owners WHERE user_id IS NULL",
        &[("org_id", false)],
    );
    // A COALESCE missing one of them proves nothing.
    assert_nullable(
        &db,
        "SELECT coalesce(email, email) AS e FROM contacts",
        &[("e", true)],
    );
}

#[test]
fn checks_that_say_nothing_about_null() {
    let db = setup();
    assert_nullable(&db, "SELECT x FROM weak WHERE id = 1", &[("x", true)]);
    assert_nullable(&db, "SELECT x FROM always", &[("x", false)]);
    assert_nullable(&db, "SELECT x FROM unvalidated_check", &[("x", false)]);
    assert_nullable(&db, "SELECT x FROM lax_check", &[("x", true)]);
    // A NO INHERIT constraint doesn't bind the children's rows a scan of
    // the parent returns.
    assert_nullable(&db, "SELECT x FROM inh_parent", &[("x", true)]);
    // Columns renamed by an alias list aren't matched to the constraints.
    assert_nullable(&db, "SELECT y FROM always AS t(i, y)", &[("y", true)]);
}

#[test]
fn checks_hold_only_where_the_row_is_there() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT coalesce(t.a_id, t.b_id) AS either FROM customers c
             LEFT JOIN things t ON t.id = c.id",
        &[("either", true)],
    );
    assert_nullable(
        &db,
        "SELECT t.a_id FROM customers c LEFT JOIN things t ON t.id = c.id WHERE t.kind = 'a'",
        &[("a_id", false)],
    );
    // A grouping set may null the column out.
    assert_nullable(
        &db,
        "SELECT kind, a_id FROM things WHERE kind = 'a' GROUP BY ROLLUP (kind, a_id)",
        &[("kind", true), ("a_id", true)],
    );
}

// ── Disjunctions in the query ────────────────────────────────────────────────

#[test]
fn disjunctions_from_the_query() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT coalesce(a, b) AS v FROM plain WHERE a IS NOT NULL OR b IS NOT NULL",
        &[("v", false)],
    );
    assert_nullable(
        &db,
        "SELECT b FROM plain WHERE (a IS NOT NULL OR b IS NOT NULL) AND a IS NULL",
        &[("b", false)],
    );
    assert_nullable(
        &db,
        "SELECT coalesce(a, b) AS v FROM plain WHERE num_nonnulls(a, b) >= 1",
        &[("v", false)],
    );
    assert_nullable(
        &db,
        "SELECT a, b FROM plain WHERE num_nulls(a, b) = 0",
        &[("a", false), ("b", false)],
    );
    assert_nullable(
        &db,
        "SELECT coalesce(a, b) AS v FROM plain WHERE a > 0 OR b > 0",
        &[("v", false)],
    );
    // Not one of them.
    assert_nullable(
        &db,
        "SELECT coalesce(a, b) AS v FROM plain WHERE a IS NOT NULL OR id > 0",
        &[("v", true)],
    );
    assert_nullable(
        &db,
        "SELECT coalesce(a, b) AS v FROM plain WHERE num_nonnulls(a, b) <= 1",
        &[("v", true)],
    );
}

// ── IN (SELECT …) ────────────────────────────────────────────────────────────

#[test]
fn in_a_subquery() {
    let db = setup();
    for qual in [
        "email IN (SELECT note FROM orders)",
        "email = ANY (SELECT note FROM orders)",
        "(email, name) IN (SELECT note, note FROM orders)",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT email FROM customers WHERE {qual}"),
            &[("email", false)],
        );
    }
    for qual in [
        // An empty subquery: `NULL NOT IN (empty)` is TRUE.
        "email NOT IN (SELECT note FROM orders)",
        "email <> ALL (SELECT note FROM orders)",
        "(email IN (SELECT note FROM orders)) IS NOT TRUE",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT email FROM customers WHERE {qual}"),
            &[("email", true)],
        );
    }
}

// ── Joins: USING and aliases ─────────────────────────────────────────────────

#[test]
fn aliased_joins_follow_their_columns() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT j.customer_id, j.note FROM (customers c LEFT JOIN orders o ON o.customer_id = c.id) AS j",
        &[("customer_id", true), ("note", true)],
    );
    // A qual on the aliased join reduces the join inside it.
    assert_nullable(
        &db,
        "SELECT j.customer_id, j.note FROM (customers c LEFT JOIN orders o ON o.customer_id = c.id) AS j
         WHERE j.note = 'a'",
        &[("customer_id", false), ("note", false)],
    );
    // A reused name doesn't loop.
    assert_nullable(
        &db,
        "SELECT t.id FROM (plain JOIN customers USING (id)) AS t",
        &[("id", false)],
    );
}

// ── HAVING proving rows ──────────────────────────────────────────────────────

#[test]
fn having_a_row_makes_aggregates_not_null() {
    let db = setup();
    assert_nullable(&db, "SELECT max(id) AS m FROM customers", &[("m", true)]);
    for having in [
        "count(*) > 0",
        "count(*) >= 1",
        "0 < count(id)",
        "count(*) = 2",
        "max(id) > 0",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT max(id) AS m FROM customers HAVING {having}"),
            &[("m", false)],
        );
    }
    for having in ["count(*) >= 0", "count(*) < 5", "max(id) IS NULL"] {
        assert_nullable(
            &db,
            &format!("SELECT max(id) AS m FROM customers HAVING {having}"),
            &[("m", true)],
        );
    }
    // A nullable argument still makes it nullable.
    assert_nullable(
        &db,
        "SELECT max(email) AS m FROM customers HAVING count(*) > 0",
        &[("m", true)],
    );
}
