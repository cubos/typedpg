//! `Rule::FkJoin` (#174): an INNER JOIN to a parent table through a
//! foreign key, whose columns nothing else reads, is dropped from the SQL
//! sent — only where PG enforces, at every statement, that each child row
//! has exactly one parent. Each negative test breaks one condition and
//! checks the SQL is sent as written.

use crate::common::*;
use typedpg_analyzer::{Rule, SimplifyOptions};

const SCHEMA: &str = "
    CREATE TABLE customers (id bigint PRIMARY KEY, name text NOT NULL, region text NOT NULL);
    CREATE TABLE orders (
        id bigint PRIMARY KEY, customer_id bigint NOT NULL REFERENCES customers,
        total numeric NOT NULL, note text
    );
    CREATE TABLE accounts (tenant int, num int, label text NOT NULL, PRIMARY KEY (tenant, num));
    CREATE TABLE entries (
        id int PRIMARY KEY, tenant int NOT NULL, num int NOT NULL, amount int NOT NULL,
        FOREIGN KEY (tenant, num) REFERENCES accounts
    );
    CREATE TABLE node (id int PRIMARY KEY, parent_id int NOT NULL REFERENCES node, name text NOT NULL);
    CREATE TABLE loose_orders (id bigint PRIMARY KEY, customer_id bigint REFERENCES customers);
    CREATE TABLE nn_orders (id bigint PRIMARY KEY, customer_id bigint REFERENCES customers);
    ALTER TABLE nn_orders ADD CONSTRAINT nn_orders_customer_nn NOT NULL customer_id NOT VALID;
    CREATE TABLE nv_orders (id bigint PRIMARY KEY, customer_id bigint NOT NULL);
    ALTER TABLE nv_orders ADD FOREIGN KEY (customer_id) REFERENCES customers NOT VALID;
    CREATE TABLE ne_orders (
        id bigint PRIMARY KEY, customer_id bigint NOT NULL,
        FOREIGN KEY (customer_id) REFERENCES customers NOT ENFORCED
    );
    CREATE TABLE def_orders (
        id bigint PRIMARY KEY,
        customer_id bigint NOT NULL REFERENCES customers DEFERRABLE INITIALLY IMMEDIATE
    );
    CREATE TABLE nv2_orders (id bigint PRIMARY KEY, customer_id bigint NOT NULL);
    ALTER TABLE nv2_orders
        ADD CONSTRAINT nv2_fk FOREIGN KEY (customer_id) REFERENCES customers NOT VALID;
    ALTER TABLE nv2_orders ALTER CONSTRAINT nv2_fk ENFORCED;
    CREATE TABLE def2_orders (
        id bigint PRIMARY KEY, customer_id bigint NOT NULL REFERENCES customers
    );
    ALTER TABLE def2_orders ALTER CONSTRAINT def2_orders_customer_id_fkey DEFERRABLE;
    CREATE TABLE secret (id bigint PRIMARY KEY);
    ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
    CREATE TABLE uses_secret (id bigint PRIMARY KEY, secret_id bigint NOT NULL REFERENCES secret);
    CREATE TABLE once_parent (id bigint PRIMARY KEY);
    CREATE TABLE once_child (id bigint PRIMARY KEY, p_id bigint NOT NULL REFERENCES once_parent);
    ALTER TABLE once_child DISABLE TRIGGER ALL;
    ALTER TABLE once_child ENABLE TRIGGER ALL;
    CREATE TABLE once_parent2 (id bigint PRIMARY KEY);
    CREATE TABLE once_child2 (id bigint PRIMARY KEY, p_id bigint NOT NULL REFERENCES once_parent2);
    ALTER TABLE once_parent2 DISABLE TRIGGER ALL;
    ALTER TABLE once_parent2 ENABLE TRIGGER ALL;
    CREATE TABLE inh_parent (id bigint PRIMARY KEY);
    CREATE TABLE inh_kid () INHERITS (inh_parent);
    CREATE TABLE refs_inh (id bigint PRIMARY KEY, p_id bigint NOT NULL REFERENCES inh_parent);
    CREATE TABLE inh_orders (id bigint PRIMARY KEY, customer_id bigint NOT NULL REFERENCES customers);
    CREATE TABLE inh_orders_kid () INHERITS (inh_orders);
    CREATE TABLE pparent (id bigint PRIMARY KEY, v int) PARTITION BY RANGE (id);
    CREATE TABLE pparent_lo PARTITION OF pparent FOR VALUES FROM (0) TO (100);
    CREATE TABLE pparent_hi PARTITION OF pparent FOR VALUES FROM (100) TO (MAXVALUE);
    CREATE TABLE porders (
        id bigint, p_id bigint NOT NULL REFERENCES pparent, PRIMARY KEY (id)
    ) PARTITION BY RANGE (id);
    CREATE TABLE porders_lo PARTITION OF porders FOR VALUES FROM (0) TO (100);
    CREATE TABLE porders_hi PARTITION OF porders FOR VALUES FROM (100) TO (MAXVALUE);
    CREATE TABLE codes (code char(3) PRIMARY KEY);
    CREATE TABLE coded (id int PRIMARY KEY, code text NOT NULL REFERENCES codes);
    CREATE TABLE notes (id int PRIMARY KEY, order_id bigint REFERENCES orders, body text);
    CREATE VIEW order_list AS
        SELECT o.id, o.total, c.name AS customer_name
        FROM orders o JOIN customers c ON c.id = o.customer_id;
    CREATE VIEW customer_view AS SELECT * FROM customers;
    CREATE TABLE regions (region text PRIMARY KEY);
    CREATE SEQUENCE seq;
";

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(SCHEMA).unwrap();
    db
}

/// The SQL the query is sent as, and the rules applied to it.
#[track_caller]
fn sent(db: &PgCatalog, sql: &str) -> (String, Vec<Rule>) {
    let a = db
        .analyze(sql)
        .unwrap_or_else(|e| panic!("`{sql}` should analyze: {e}"));
    (a.sql, a.simplifications.iter().map(|s| s.rule).collect())
}

#[track_caller]
fn assert_simplified(db: &PgCatalog, cases: &[(&str, &str)]) {
    for (sql, expected) in cases {
        let (sql_sent, rules) = sent(db, sql);
        assert_eq!(sql_sent, *expected, "`{sql}`");
        assert!(
            !rules.is_empty() && rules.iter().all(|r| *r == Rule::FkJoin),
            "`{sql}`: {rules:?}"
        );
    }
}

#[track_caller]
fn assert_kept(db: &PgCatalog, sqls: &[&str]) {
    for sql in sqls {
        let (sql_sent, rules) = sent(db, sql);
        assert!(rules.is_empty(), "`{sql}` was rewritten to `{sql_sent}`");
    }
}

// ─── Positive shapes ──────────────────────────────────────────────────────

#[test]
fn join_on_the_key_is_dropped() {
    let db = setup();
    assert_simplified(
        &db,
        &[
            (
                "SELECT o.id, o.total FROM orders o JOIN customers c ON c.id = o.customer_id \
                 WHERE o.total < 100",
                "SELECT o.id, o.total FROM orders o WHERE o.total < 100",
            ),
            (
                "SELECT o.id FROM orders o INNER JOIN customers c ON o.customer_id = c.id",
                "SELECT o.id FROM orders o",
            ),
            (
                "SELECT o.id FROM orders o JOIN customers AS c ON (c.id = o.customer_id) \
                 ORDER BY o.id",
                "SELECT o.id FROM orders o ORDER BY o.id",
            ),
            (
                "SELECT orders.id FROM orders JOIN public.customers \
                 ON customers.id = orders.customer_id",
                "SELECT orders.id FROM orders",
            ),
            (
                "SELECT count(*) FROM orders o JOIN customers c ON c.id = o.customer_id",
                "SELECT count(*) FROM orders o",
            ),
        ],
    );
}

#[test]
fn composite_key_join_is_dropped() {
    let db = setup();
    assert_simplified(
        &db,
        &[
            (
                "SELECT e.amount FROM entries e \
                 JOIN accounts a ON a.tenant = e.tenant AND a.num = e.num",
                "SELECT e.amount FROM entries e",
            ),
            (
                "SELECT e.amount FROM entries e \
                 JOIN accounts a ON (e.num = a.num AND (e.tenant = a.tenant)) WHERE e.id > 1",
                "SELECT e.amount FROM entries e WHERE e.id > 1",
            ),
        ],
    );
}

#[test]
fn comma_join_on_the_key_is_dropped() {
    let db = setup();
    assert_simplified(
        &db,
        &[
            (
                "SELECT o.id FROM orders o, customers c WHERE c.id = o.customer_id",
                "SELECT o.id FROM orders o",
            ),
            (
                "SELECT o.id FROM customers c, orders o WHERE o.customer_id = c.id \
                 ORDER BY o.id",
                "SELECT o.id FROM orders o ORDER BY o.id",
            ),
            (
                "SELECT o.id FROM orders o, customers c \
                 WHERE o.total > 0 AND c.id = o.customer_id",
                "SELECT o.id FROM orders o WHERE o.total > 0",
            ),
            (
                "SELECT o.id FROM orders o, customers c \
                 WHERE c.id = o.customer_id AND o.total > 0 AND o.id < 9",
                "SELECT o.id FROM orders o WHERE o.total > 0 AND o.id < 9",
            ),
            (
                "SELECT e.id FROM entries e, accounts a \
                 WHERE (a.tenant = e.tenant) AND e.amount > 0 AND a.num = e.num",
                "SELECT e.id FROM entries e WHERE e.amount > 0",
            ),
        ],
    );
}

#[test]
fn several_parents_and_nested_joins() {
    let db = setup();
    assert_simplified(
        &db,
        &[
            // Both joins go, one at a time.
            (
                "SELECT n.id FROM notes n JOIN orders o ON o.id = n.id::bigint \
                 JOIN customers c ON c.id = o.customer_id",
                "SELECT n.id FROM notes n JOIN orders o ON o.id = n.id::bigint",
            ),
            // The child is the preserved side of a LEFT JOIN.
            (
                "SELECT o.id, n.body FROM orders o LEFT JOIN notes n ON n.order_id = o.id \
                 JOIN customers c ON c.id = o.customer_id",
                "SELECT o.id, n.body FROM orders o LEFT JOIN notes n ON n.order_id = o.id",
            ),
            // A self-reference.
            (
                "SELECT n.name FROM node n JOIN node p ON p.id = n.parent_id",
                "SELECT n.name FROM node n",
            ),
        ],
    );
}

#[test]
fn parameters_are_kept() {
    let db = setup();
    assert_simplified(
        &db,
        &[(
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id \
             WHERE o.total < $max",
            "SELECT o.id FROM orders o WHERE o.total < $1",
        )],
    );
}

#[test]
fn partitioned_tables_and_only() {
    let db = setup();
    assert_simplified(
        &db,
        &[
            // A partitioned parent: the key looks rows up in all of it.
            (
                "SELECT r.id FROM porders r JOIN pparent p ON p.id = r.p_id",
                "SELECT r.id FROM porders r",
            ),
            // ONLY a plain parent with inheritance children: the key's own
            // rows, unique there.
            (
                "SELECT r.id FROM refs_inh r JOIN ONLY inh_parent p ON p.id = r.p_id",
                "SELECT r.id FROM refs_inh r",
            ),
            // ONLY a child with inheritance children: rows the key covers.
            (
                "SELECT o.id FROM ONLY inh_orders o JOIN customers c ON c.id = o.customer_id",
                "SELECT o.id FROM ONLY inh_orders o",
            ),
        ],
    );
}

// ─── Negative: one condition broken each ──────────────────────────────────

#[test]
fn condition_must_be_exactly_the_key() {
    let db = setup();
    assert_kept(
        &db,
        &[
            // Something else in the ON.
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id AND o.total > 0",
            "SELECT o.id FROM orders o JOIN customers c \
             ON c.id = o.customer_id AND c.region = 'x'",
            // Not the key's columns.
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.id",
            // Part of a composite key.
            "SELECT e.id FROM entries e JOIN accounts a ON a.tenant = e.tenant",
            "SELECT e.id FROM entries e, accounts a WHERE a.tenant = e.tenant",
            // Pairs crossed over.
            "SELECT e.id FROM entries e JOIN accounts a ON a.tenant = e.num AND a.num = e.tenant",
            // Not the key's equality operator: `text = text` over a char(3)
            // key compares the trailing blanks bpchareq ignores.
            "SELECT d.id FROM coded d JOIN codes c ON c.code = d.code",
            // An expression, not the column.
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id + 0",
            // An OR in the WHERE of the comma form.
            "SELECT o.id FROM orders o, customers c WHERE c.id = o.customer_id OR o.total > 0",
        ],
    );
}

#[test]
fn referencing_columns_must_be_not_null() {
    let db = setup();
    assert_kept(
        &db,
        &[
            "SELECT o.id FROM loose_orders o JOIN customers c ON c.id = o.customer_id",
            "SELECT o.id FROM loose_orders o, customers c WHERE c.id = o.customer_id",
            // A NOT VALID not-null constraint: older rows may be NULL.
            "SELECT o.id FROM nn_orders o JOIN customers c ON c.id = o.customer_id",
        ],
    );
}

#[test]
fn key_must_be_validated_enforced_and_immediate() {
    let db = setup();
    assert_kept(
        &db,
        &[
            "SELECT o.id FROM nv_orders o JOIN customers c ON c.id = o.customer_id",
            "SELECT o.id FROM ne_orders o JOIN customers c ON c.id = o.customer_id",
            "SELECT o.id FROM def_orders o JOIN customers c ON c.id = o.customer_id",
            // ENFORCED on an enforced NOT VALID key leaves it NOT VALID.
            "SELECT o.id FROM nv2_orders o JOIN customers c ON c.id = o.customer_id",
            // Made DEFERRABLE by ALTER CONSTRAINT.
            "SELECT o.id FROM def2_orders o JOIN customers c ON c.id = o.customer_id",
        ],
    );
}

#[test]
fn key_validated_without_checking_the_rows_is_not_trusted() {
    // PG 18 rewrites an enforced NOT VALID key's row as validated, without
    // checking the rows, when ENFORCED comes with a deferrability change;
    // NOT DEFERRABLE again, it looks enforced, valid and immediate while
    // orphans may remain.
    let tables = "CREATE TABLE cust (id bigint PRIMARY KEY, name text);
         CREATE TABLE ord (id bigint PRIMARY KEY, cid bigint NOT NULL);";
    let sql = "SELECT o.id FROM ord o JOIN cust c ON c.id = o.cid";
    for steps in [
        "ALTER TABLE ord ADD CONSTRAINT f FOREIGN KEY (cid) REFERENCES cust NOT VALID DEFERRABLE;
         ALTER TABLE ord ALTER CONSTRAINT f ENFORCED NOT DEFERRABLE;",
        "ALTER TABLE ord ADD CONSTRAINT f FOREIGN KEY (cid) REFERENCES cust NOT VALID;
         ALTER TABLE ord ALTER CONSTRAINT f ENFORCED DEFERRABLE;
         ALTER TABLE ord ALTER CONSTRAINT f NOT DEFERRABLE;",
        // VALIDATE CONSTRAINT skips a key already marked valid.
        "ALTER TABLE ord ADD CONSTRAINT f FOREIGN KEY (cid) REFERENCES cust NOT VALID;
         ALTER TABLE ord ALTER CONSTRAINT f ENFORCED DEFERRABLE;
         ALTER TABLE ord ALTER CONSTRAINT f NOT DEFERRABLE;
         ALTER TABLE ord VALIDATE CONSTRAINT f;",
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(tables).unwrap();
        db.apply_sql(steps).unwrap();
        assert_kept(&db, &[sql]);
    }
    // A partitioned child: its partitions' clones too.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE cust (id bigint PRIMARY KEY, name text);
         CREATE TABLE pord (id bigint, cid bigint NOT NULL) PARTITION BY RANGE (id);
         CREATE TABLE pord1 PARTITION OF pord FOR VALUES FROM (0) TO (100);
         ALTER TABLE pord ADD CONSTRAINT pf FOREIGN KEY (cid) REFERENCES cust NOT VALID;
         ALTER TABLE pord ALTER CONSTRAINT pf ENFORCED DEFERRABLE;
         ALTER TABLE pord ALTER CONSTRAINT pf NOT DEFERRABLE;",
    )
    .unwrap();
    assert_kept(
        &db,
        &["SELECT o.id FROM pord o JOIN cust c ON c.id = o.cid"],
    );
    // NOT ENFORCED, then ENFORCED: the rows are checked.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(tables).unwrap();
    db.apply_sql(
        "ALTER TABLE ord ADD CONSTRAINT f FOREIGN KEY (cid) REFERENCES cust NOT VALID;
         ALTER TABLE ord ALTER CONSTRAINT f ENFORCED DEFERRABLE;
         ALTER TABLE ord ALTER CONSTRAINT f NOT DEFERRABLE;
         ALTER TABLE ord ALTER CONSTRAINT f NOT ENFORCED;
         ALTER TABLE ord ALTER CONSTRAINT f ENFORCED;",
    )
    .unwrap();
    assert_simplified(&db, &[(sql, "SELECT o.id FROM ord o")]);
}

#[test]
fn parent_must_not_have_row_security() {
    let db = setup();
    assert_kept(
        &db,
        &["SELECT u.id FROM uses_secret u JOIN secret s ON s.id = u.secret_id"],
    );
}

#[test]
fn triggers_must_never_have_been_disabled() {
    let db = setup();
    assert_kept(
        &db,
        &[
            // On the child, enabled again since.
            "SELECT c.id FROM once_child c JOIN once_parent p ON p.id = c.p_id",
            // On the parent, enabled again since.
            "SELECT c.id FROM once_child2 c JOIN once_parent2 p ON p.id = c.p_id",
        ],
    );
}

#[test]
fn session_replication_role_voids_the_proof() {
    let tables = "CREATE TABLE customers (id bigint PRIMARY KEY);
         CREATE TABLE orders (
             id bigint PRIMARY KEY, customer_id bigint NOT NULL REFERENCES customers
         );";
    let sql = "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id";
    for setting in [
        "SET session_replication_role = replica; SET session_replication_role = DEFAULT;",
        "SELECT set_config('session_replication_role', 'replica', false);",
        // set_config of a setting that isn't a constant.
        "SELECT set_config(v.name, 'x', false) FROM (VALUES ('application_name')) v (name);",
        "PREPARE p (text) AS SELECT set_config($1, 'x', true);",
        // A routine running with it.
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
         SET session_replication_role = replica AS 'SELECT 1';",
        "CREATE PROCEDURE pr() LANGUAGE sql AS 'SELECT 1'; \
         ALTER PROCEDURE pr() SET session_replication_role = replica;",
        // A write to a system catalog.
        "UPDATE pg_trigger SET tgenabled = 'D' WHERE false;",
        "UPDATE pg_catalog.pg_constraint SET convalidated = true WHERE false;",
        // The setting's name escaped: compared decoded.
        "SET U&\"session\\005freplication\\005frole\" = replica; \
         RESET U&\"session\\005freplication\\005frole\";",
        "SELECT set_config(U&'session\\005freplication\\005frole', 'origin', false);",
        // A routine or DO block whose body may turn the checks off,
        // defined — however (and whether) it is ever run.
        "CREATE FUNCTION off() RETURNS void LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN EXECUTE 'ALTER TABLE orders DISABLE TRIGGER ALL'; END IF; END $$;",
        "CREATE PROCEDURE off() LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN UPDATE pg_trigger SET tgenabled = 'D'; END IF; END $$;",
        "CREATE FUNCTION off() RETURNS text LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN PERFORM set_config('session_replication' || '_role', 'replica', \
         false); END IF; RETURN ''; END $$;",
        "DO $$ BEGIN IF false THEN EXECUTE 'ALTER TABLE orders DISABLE TRIGGER ALL'; \
         END IF; END $$;",
        // `DISABLE TRIGGER` split by a newline, a tab, a comment.
        "CREATE FUNCTION off() RETURNS void LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN ALTER TABLE orders DISABLE\nTRIGGER ALL; END IF; END $$;",
        "CREATE FUNCTION off() RETURNS void LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN ALTER TABLE orders DISABLE\tTRIGGER ALL; END IF; END $$;",
        "CREATE FUNCTION off() RETURNS void LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN ALTER TABLE orders DISABLE /**/ TRIGGER ALL; END IF; END $$;",
        // A SQL body, BEGIN ATOMIC, a catalog named with `U&` escapes.
        "CREATE FUNCTION n() RETURNS bigint LANGUAGE sql \
         AS $$ SELECT count(*) FROM U&\"\\0070g_class\" $$;",
        "CREATE FUNCTION n() RETURNS bigint LANGUAGE sql \
         BEGIN ATOMIC SELECT count(*) FROM pg_catalog.pg_constraint; END;",
        // Reached through a column DEFAULT, replaced afterwards.
        "CREATE FUNCTION d() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
         CREATE TABLE with_default (a int DEFAULT d()); \
         CREATE OR REPLACE FUNCTION d() RETURNS int LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN EXECUTE 'SELECT 1'; END IF; RETURN 1; END $$;",
        // Through an operator.
        "CREATE FUNCTION opf(int, int) RETURNS bool LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN PERFORM set_config('a.b', 'c', false); END IF; RETURN true; END $$; \
         CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = opf);",
        // Through a trigger a cascade fires.
        "CREATE TABLE lines (id int PRIMARY KEY, \
         order_id bigint NOT NULL REFERENCES orders ON DELETE CASCADE); \
         CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF false THEN EXECUTE 'SELECT 1'; END IF; RETURN OLD; END $$; \
         CREATE TRIGGER t BEFORE DELETE ON lines FOR EACH ROW EXECUTE FUNCTION tf();",
        // Writes through a view over a catalog.
        "CREATE VIEW trg AS SELECT oid, tgenabled FROM pg_catalog.pg_trigger; \
         UPDATE trg SET tgenabled = 'D' WHERE false;",
        "CREATE VIEW trg AS SELECT oid FROM pg_trigger; DELETE FROM trg WHERE false;",
        "CREATE VIEW cls AS SELECT oid, relhastriggers FROM pg_catalog.pg_class; \
         UPDATE cls SET relhastriggers = false WHERE false;",
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(tables).unwrap();
        db.apply_sql(setting).unwrap();
        assert_kept(&db, &[sql]);
    }
    // Code without any of the markers leaves the proof alone.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(tables).unwrap();
    db.apply_sql(
        "CREATE FUNCTION add_one(i int) RETURNS int LANGUAGE plpgsql AS $$ \
         BEGIN -- a comment\n RETURN i + 1; END $$; \
         CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.id := NEW.id; RETURN NEW; END $$; \
         CREATE TRIGGER t BEFORE INSERT ON orders FOR EACH ROW EXECUTE FUNCTION touch(); \
         DO $$ BEGIN PERFORM add_one(1); END $$;",
    )
    .unwrap();
    assert_simplified(&db, &[(sql, "SELECT o.id FROM orders o")]);
    // Settings for other sessions: not run on the oracle's shared cluster.
    for setting in [
        "ALTER ROLE postgres SET session_replication_role = replica;",
        "ALTER DATABASE postgres SET session_replication_role = replica;",
        "ALTER SYSTEM SET session_replication_role = replica;",
        "ALTER ROLE postgres SET U&\"session\\005freplication\\005frole\" = replica;",
        // Logical replication's apply workers run as replicas.
        "CREATE SUBSCRIPTION s CONNECTION 'dbname=x' PUBLICATION p WITH (connect = false);",
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.skip_pg_sanity();
        db.apply_sql(tables).unwrap();
        db.apply_sql(setting).unwrap();
        assert_kept(&db, &[sql]);
    }
    // COPY into or TRUNCATE a system catalog (allow_system_table_mods):
    // either the migration is rejected, so no schema is built past it, or
    // the proof is void.
    for setting in [
        "COPY pg_catalog.pg_trigger FROM STDIN;",
        "TRUNCATE pg_catalog.pg_trigger;",
    ] {
        let mut db = PgCatalog::new().unwrap();
        db.skip_pg_sanity();
        db.apply_sql(tables).unwrap();
        match db.apply_sql(setting) {
            Ok(()) => assert_kept(&db, &[sql]),
            Err(e) => assert!(
                e.to_string().contains("is a system catalog"),
                "{setting}: {e}"
            ),
        }
    }
}

#[test]
fn parent_columns_must_not_be_used() {
    let db = setup();
    let join = "FROM orders o JOIN customers c ON c.id = o.customer_id";
    let queries = [
        format!("SELECT o.id, c.name {join}"),
        format!("SELECT o.id {join} WHERE c.region = 'x'"),
        format!("SELECT o.id {join} ORDER BY c.name"),
        format!("SELECT c.region, count(*) {join} GROUP BY c.region"),
        format!("SELECT count(*) {join} GROUP BY o.id HAVING max(c.name) > 'a'"),
        format!("SELECT o.id, row_number() OVER (PARTITION BY c.region) {join}"),
        format!("SELECT o.id, rank() OVER w {join} WINDOW w AS (ORDER BY c.name)"),
        format!("SELECT c.* {join}"),
        format!("SELECT o.id, c {join}"),
        format!("SELECT o.id, (c).name {join}"),
        format!("SELECT o.id, to_jsonb(c) {join}"),
        format!("SELECT * {join}"),
        // Unqualified: `region` is only the parent's.
        format!("SELECT o.id {join} WHERE region = 'x'"),
        format!("SELECT o.id {join} WHERE EXISTS (SELECT 1 FROM notes n WHERE n.body = c.name)"),
        format!("SELECT DISTINCT ON (c.region) o.id {join}"),
        format!("SELECT c.id {join}"),
        // In another join's condition.
        format!("SELECT o.id {join} JOIN notes n ON n.body = c.name"),
        // The other key column of the comma form, and a WHERE using it.
        "SELECT o.id FROM orders o, customers c WHERE c.id = o.customer_id AND c.name <> ''"
            .to_string(),
    ];
    let refs: Vec<&str> = queries.iter().map(String::as_str).collect();
    assert_kept(&db, &refs);
}

#[test]
fn row_locking_keeps_the_join() {
    let db = setup();
    assert_kept(
        &db,
        &[
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id FOR UPDATE",
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id FOR SHARE OF o",
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id \
             FOR KEY SHARE OF c",
        ],
    );
}

#[test]
fn inheritance_and_only_must_match_the_key() {
    let db = setup();
    assert_kept(
        &db,
        &[
            // The parent's inheritance children may repeat its key.
            "SELECT r.id FROM refs_inh r JOIN inh_parent p ON p.id = r.p_id",
            // ONLY a partitioned table scans nothing.
            "SELECT r.id FROM porders r JOIN ONLY pparent p ON p.id = r.p_id",
            // The child's inheritance children don't carry its key.
            "SELECT o.id FROM inh_orders o JOIN customers c ON c.id = o.customer_id",
        ],
    );
}

#[test]
fn volatile_functions_keep_the_join() {
    let db = setup();
    assert_kept(
        &db,
        &[
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id \
             WHERE random() < 0.5",
            "SELECT o.id, nextval('seq') FROM orders o JOIN customers c ON c.id = o.customer_id",
        ],
    );
}

#[test]
fn only_inner_joins_on_a_child_that_is_never_null_extended() {
    let db = setup();
    assert_kept(
        &db,
        &[
            "SELECT o.id FROM orders o LEFT JOIN customers c ON c.id = o.customer_id",
            "SELECT o.id FROM customers c RIGHT JOIN orders o ON c.id = o.customer_id",
            // The child is NULL-extended by the LEFT JOIN before it.
            "SELECT n.id FROM notes n LEFT JOIN orders o ON o.id = n.order_id \
             JOIN customers c ON c.id = o.customer_id",
            // The parent on the left of the JOIN (not a shape rewritten).
            "SELECT o.id FROM customers c JOIN orders o ON c.id = o.customer_id",
        ],
    );
}

#[test]
fn only_plain_queries_over_base_tables() {
    let db = setup();
    assert_kept(
        &db,
        &[
            // Through a view: inlining it would change whose privileges apply.
            "SELECT id, total FROM order_list WHERE total < 100",
            "SELECT o.id FROM orders o JOIN customer_view c ON c.id = o.customer_id",
            // In a subquery, a CTE, a set operation.
            "SELECT s.id FROM (SELECT o.id FROM orders o \
             JOIN customers c ON c.id = o.customer_id) s",
            "WITH x AS (SELECT 1) SELECT o.id FROM orders o \
             JOIN customers c ON c.id = o.customer_id",
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id \
             UNION ALL SELECT 1",
            // USING elsewhere may merge a parent column by name.
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id \
             JOIN regions r USING (region)",
            // DML.
            "DELETE FROM notes n USING orders o JOIN customers c ON c.id = o.customer_id \
             WHERE n.order_id = o.id",
        ],
    );
}

#[test]
fn cuts_never_touch_a_comment() {
    let db = setup();
    // A cut starting right after a `--` comment would take its newline and
    // comment out the rest of the query; one holding or touching any
    // comment is not made.
    assert_kept(
        &db,
        &[
            // JOIN form.
            "SELECT o.id FROM orders o -- hi\nJOIN customers c ON c.id = o.customer_id \
             WHERE o.total < 5",
            "SELECT o.id FROM orders o /* the parent */ JOIN customers c \
             ON c.id = o.customer_id WHERE o.total < 5",
            "SELECT o.id FROM orders o JOIN customers c -- WHERE c.region = 'x'\n\
             ON c.id = o.customer_id WHERE o.total < 5",
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id/* ; */\n\
             WHERE o.total < 5",
            // Comma form: the FROM item, the whole WHERE.
            "SELECT o.id FROM orders o -- hi\n, customers c \
             WHERE c.id = o.customer_id AND o.total < 5",
            "SELECT o.id FROM orders o, customers c -- note\nWHERE c.id = o.customer_id",
            // The trailing ` AND` cut.
            "SELECT o.id FROM orders o, customers c \
             WHERE o.total < 5 -- AND o.id = 1\nAND c.id = o.customer_id",
            "SELECT o.id FROM orders o, customers c \
             WHERE o.total < 5 /* AND c.region = 'x' */ AND c.id = o.customer_id",
            // The leading `... AND` cut.
            "SELECT o.id FROM orders o, customers c \
             WHERE c.id = o.customer_id /* x */ AND o.total < 5",
        ],
    );
    // Comments away from the cut stay as they are.
    assert_simplified(
        &db,
        &[
            (
                "SELECT o.id -- ids\nFROM orders o JOIN customers c ON c.id = o.customer_id \
                 -- WHERE false\nWHERE o.total < 5",
                "SELECT o.id -- ids\nFROM orders o -- WHERE false\nWHERE o.total < 5",
            ),
            (
                "SELECT o.id FROM orders o, customers c /* c */ \
                 WHERE o.total < 5 AND c.id = o.customer_id -- end",
                "SELECT o.id FROM orders o /* c */ WHERE o.total < 5 -- end",
            ),
        ],
    );
}

#[test]
fn rule_can_be_turned_off() {
    let mut db = setup();
    let sql = "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id";
    db.set_simplify_options(SimplifyOptions {
        enabled: true,
        disabled: [Rule::FkJoin].into_iter().collect(),
    });
    assert_kept(&db, &[sql]);
    db.set_simplify_options(SimplifyOptions {
        enabled: false,
        disabled: Default::default(),
    });
    assert_kept(&db, &[sql]);
    db.set_simplify_options(SimplifyOptions::default());
    assert_simplified(&db, &[(sql, "SELECT o.id FROM orders o")]);
}
