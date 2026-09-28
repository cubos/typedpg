//! DDL integration miscellany: snapshot JSON roundtrip, multi-statement
//! real-world migrations, DML statements appearing in migration files, and
//! other behaviours that don't fit a single DDL feature.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL
        );",
    )
    .unwrap();
    db
}

// ── Snapshot JSON roundtrip ──────────────────────────────────────────────────

#[test]
fn snapshot_roundtrip() {
    let db = setup();
    let seed = db.to_seed();

    let json = serde_json::to_string(&seed).unwrap();
    let restored: PgCatalogSeed = serde_json::from_str(&json).unwrap();

    assert_eq!(db.pg_type().len(), restored.pg_type.len());
    assert_eq!(db.pg_class().len(), restored.pg_class.len());
    assert_eq!(db.pg_proc().len(), restored.pg_proc.len());
    assert_eq!(db.pg_operator().len(), restored.pg_operator.len());
    assert_eq!(db.pg_cast().len(), restored.pg_cast.len());

    // Analyze against both databases — results must match exactly.
    let restored_db = PgCatalog::from_seed(restored);
    let sql = "SELECT id, name FROM users";
    let info1 = db.analyze(sql).unwrap();
    let info2 = restored_db.analyze(sql).unwrap();
    assert_identical(&info1, &info2, "snapshot roundtrip");
}

// ── DML mixed into migration files is silently ignored ────────────────────

#[test]
fn dml_statements_in_migration_are_ignored() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO t (name) VALUES ('seed');
         UPDATE t SET name = 'updated' WHERE id = 1;
         DELETE FROM t WHERE id = 999;",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(snap.attributes_of(table.oid).len(), 2);
}

// ── Multi-file real-world migration chain ─────────────────────────────────

#[test]
fn complex_real_world_migration_chain() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\";

             CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');

             CREATE TABLE organizations (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 name TEXT NOT NULL,
                 slug TEXT NOT NULL UNIQUE,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );

             CREATE TABLE users (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
                 email TEXT NOT NULL,
                 name TEXT NOT NULL,
                 role user_role NOT NULL DEFAULT 'viewer',
                 active BOOLEAN NOT NULL DEFAULT true,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                 UNIQUE (org_id, email)
             );

             CREATE INDEX idx_users_org_id ON users (org_id);
             CREATE INDEX idx_users_email ON users (email);",
        ),
        (
            "0002.sql",
            "CREATE TABLE projects (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 org_id UUID NOT NULL REFERENCES organizations(id),
                 name TEXT NOT NULL,
                 description TEXT,
                 archived BOOLEAN NOT NULL DEFAULT false,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );

             CREATE TABLE tasks (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 assigned_to UUID REFERENCES users(id),
                 title TEXT NOT NULL,
                 body TEXT,
                 priority INT NOT NULL DEFAULT 0 CHECK (priority >= 0),
                 completed_at TIMESTAMPTZ,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );

             CREATE VIEW active_tasks AS
                 SELECT t.id, t.title, t.priority, p.name AS project_name,
                        u.name AS assignee_name
                 FROM tasks t
                 JOIN projects p ON p.id = t.project_id
                 LEFT JOIN users u ON u.id = t.assigned_to
                 WHERE t.completed_at IS NULL AND NOT p.archived;",
        ),
        (
            "0003.sql",
            "ALTER TYPE user_role ADD VALUE 'owner' BEFORE 'admin';
             ALTER TABLE users ADD COLUMN last_login_at TIMESTAMPTZ;
             ALTER TABLE projects ADD COLUMN owner_id UUID REFERENCES users(id);",
        ),
    ]);

    // Verify organizations.
    let orgs = snap.resolve_table(None, "organizations").unwrap();
    let org_attrs = snap.attributes_of(orgs.oid);
    assert_eq!(org_attrs.len(), 4);
    let org_id = org_attrs.iter().find(|c| c.attname == "id").unwrap();
    assert!(org_id.attnotnull);
    assert!(org_id.atthasdef);

    // Verify users (8 columns after ALTER ADD COLUMN).
    let users = snap.resolve_table(None, "users").unwrap();
    let user_attrs = snap.attributes_of(users.oid);
    assert_eq!(user_attrs.len(), 8);
    let role_col = user_attrs.iter().find(|c| c.attname == "role").unwrap();
    let role_type = snap.get_type(role_col.atttypid).unwrap();
    assert_eq!(role_type.typtype, TypType::Enum);

    // user_role gained a new label at the top.
    let labels = snap.enum_labels_of(role_type.oid);
    assert_eq!(labels, vec!["owner", "admin", "editor", "viewer"]);

    // Verify the active_tasks view has the right shape.
    let view = snap.resolve_table(None, "active_tasks").unwrap();
    let view_attrs = snap.attributes_of(view.oid);
    assert_eq!(view_attrs.len(), 5);
    assert_eq!(view_attrs[0].attname, "id");
    assert_eq!(view_attrs[1].attname, "title");
    assert_eq!(view_attrs[3].attname, "project_name");
    assert_eq!(view_attrs[4].attname, "assignee_name");

    // Tasks table.
    let tasks = snap.resolve_table(None, "tasks").unwrap();
    let task_attrs = snap.attributes_of(tasks.oid);
    assert_eq!(task_attrs.len(), 8);
    let priority = task_attrs.iter().find(|c| c.attname == "priority").unwrap();
    assert!(priority.attnotnull);
    assert!(priority.atthasdef);

    // Projects with added column.
    let projects = snap.resolve_table(None, "projects").unwrap();
    assert_eq!(snap.attributes_of(projects.oid).len(), 7);
}

// ── Statements PG accepts that don't reshape the catalog ────────────────────

#[test]
fn common_migration_statements_are_accepted() {
    // PG 18 accepts each of these; none changes a relation's shape.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int);
         CREATE MATERIALIZED VIEW mv AS SELECT a FROM t;
         CREATE PROCEDURE p(x int) LANGUAGE sql AS 'insert into t values (x)';
         REFRESH MATERIALIZED VIEW mv;
         REFRESH MATERIALIZED VIEW mv WITH NO DATA;
         CALL p(1);
         MERGE INTO t USING (SELECT 1 x) s ON t.a = s.x
             WHEN NOT MATCHED THEN INSERT VALUES (s.x);
         CREATE STATISTICS st ON a, b FROM t;
         ALTER STATISTICS st SET STATISTICS 100;
         ALTER DATABASE postgres SET timezone TO 'UTC';
         ALTER DATABASE postgres RESET timezone;
         CREATE ROLE r;
         ALTER ROLE r SET search_path = public;
         REASSIGN OWNED BY r TO postgres;
         DROP OWNED BY r;
         DROP ROLE r;
         PREPARE q AS SELECT 1;
         EXECUTE q;
         DEALLOCATE q;
         CREATE PUBLICATION pub FOR TABLE t;
         CREATE OPERATOR FAMILY f USING btree;
         CREATE TEXT SEARCH CONFIGURATION my_cfg (COPY = english);
         ALTER TEXT SEARCH CONFIGURATION my_cfg ALTER MAPPING FOR word WITH simple;
         CHECKPOINT;",
    )]);
}

#[test]
fn refresh_materialized_view_requires_one() {
    // PG 18: 0A000 "t" is not a materialized view; 42P01 for a missing one.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (a int); REFRESH MATERIALIZED VIEW t;"
        )]),
        DdlError::Parse(_),
        "\"t\" is not a materialized view",
    );
    assert_ddl_err!(
        try_apply(&[("0001.sql", "REFRESH MATERIALIZED VIEW nosuch;")]),
        DdlError::TableNotFound(_),
        "relation \"nosuch\" does not exist",
    );
}

#[test]
fn foreign_tables_are_relations() {
    // PG 18: the foreign table has its declared columns; DROP TABLE on it is
    // refused, DROP FOREIGN TABLE works.
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE FOREIGN DATA WRAPPER w;
         CREATE SERVER srv FOREIGN DATA WRAPPER w;
         CREATE FOREIGN TABLE ft (a int NOT NULL, b text) SERVER srv;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM ft").unwrap(),
        vec![c("a", int4()), cn("b", text())],
    );
    let err = db.apply_sql("DROP TABLE ft;").unwrap_err();
    assert!(
        err.to_string().starts_with("\"ft\" is not a table"),
        "{err}"
    );
    db.apply_sql("DROP FOREIGN TABLE ft;").unwrap();
}

// ── COMMENT ON resolves its target (get_object_address) ─────────────────────

#[test]
fn comment_on_requires_an_existing_target() {
    let setup = "CREATE TABLE t (a int);";
    for (stmt, msg) in [
        (
            "COMMENT ON TABLE nosuch IS 'x';",
            "relation \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON COLUMN t.nosuch IS 'x';",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "COMMENT ON COLUMN nosuch.a IS 'x';",
            "relation \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON TYPE nosuch IS 'x';",
            "type \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON FUNCTION nosuch(int) IS 'x';",
            "function nosuch(integer) does not exist",
        ),
        (
            "COMMENT ON SCHEMA nosuch IS 'x';",
            "schema \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON CONSTRAINT nosuch ON t IS 'x';",
            "constraint \"nosuch\" for table \"t\" does not exist",
        ),
        ("COMMENT ON VIEW t IS 'x';", "\"t\" is not a view"),
        (
            "COMMENT ON INDEX nosuch IS 'x';",
            "relation \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int PRIMARY KEY);
         COMMENT ON TABLE t IS 'x';
         COMMENT ON COLUMN t.a IS NULL;
         COMMENT ON CONSTRAINT t_pkey ON t IS 'pk';
         COMMENT ON SCHEMA public IS 'p';",
    )]);
}

#[test]
fn grant_targets_must_exist() {
    // PG 18 resolves every object a GRANT / REVOKE names.
    let setup = "CREATE TABLE t (a int);";
    for (stmt, msg) in [
        (
            "GRANT SELECT ON nosuch TO public;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "REVOKE ALL ON nosuch FROM public;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "GRANT SELECT ON ALL TABLES IN SCHEMA nosch TO public;",
            "schema \"nosch\" does not exist",
        ),
        (
            "GRANT EXECUTE ON FUNCTION nosuch(int) TO public;",
            "function nosuch(integer) does not exist",
        ),
        (
            "GRANT USAGE ON SCHEMA nosch TO public;",
            "schema \"nosch\" does not exist",
        ),
        (
            "GRANT USAGE ON SEQUENCE nosuch TO public;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "GRANT SELECT (nosuchcol) ON t TO public;",
            "column \"nosuchcol\" of relation \"t\" does not exist",
        ),
        (
            "GRANT USAGE ON TYPE nosuch TO public;",
            "type \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int); CREATE SEQUENCE s;
         CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
         GRANT SELECT, INSERT ON t TO public;
         GRANT SELECT (a) ON t TO public;
         GRANT USAGE ON SEQUENCE s TO public;
         GRANT EXECUTE ON FUNCTION f(int) TO public;
         GRANT EXECUTE ON FUNCTION f TO public;
         GRANT USAGE ON SCHEMA public TO public;
         GRANT SELECT ON ALL TABLES IN SCHEMA public TO public;
         REVOKE ALL ON t FROM public;",
    )]);
}

#[test]
fn policies_are_validated_and_tracked() {
    // PG 18 CreatePolicy / AlterPolicy / rename_policy / DROP POLICY.
    let setup = "CREATE TABLE t (a int);
                 CREATE POLICY p ON t USING (a > 0);
                 CREATE POLICY p2 ON t USING (true);
                 CREATE SEQUENCE s;
                 CREATE VIEW v AS SELECT 1 AS x;";
    for (stmt, msg) in [
        (
            "CREATE POLICY q ON nosuch USING (true);",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE POLICY q ON t USING (a);",
            "argument of POLICY must be type boolean, not type integer",
        ),
        (
            "CREATE POLICY q ON t USING (a > 0) WITH CHECK (a);",
            "argument of POLICY must be type boolean, not type integer",
        ),
        (
            "CREATE POLICY q ON t USING (nosuchcol > 0);",
            "column \"nosuchcol\" does not exist",
        ),
        (
            "CREATE POLICY q ON t USING (sum(a) > 0);",
            "aggregate functions are not allowed in policy expressions",
        ),
        (
            "CREATE POLICY p ON t USING (true);",
            "policy \"p\" for table \"t\" already exists",
        ),
        ("CREATE POLICY q ON s USING (true);", "\"s\" is not a table"),
        ("CREATE POLICY q ON v USING (true);", "\"v\" is not a table"),
        (
            "ALTER POLICY nosuch ON t USING (true);",
            "policy \"nosuch\" for table \"t\" does not exist",
        ),
        (
            "ALTER POLICY p ON t USING (a);",
            "argument of POLICY must be type boolean, not type integer",
        ),
        (
            "DROP POLICY nosuch ON t;",
            "policy \"nosuch\" for table \"t\" does not exist",
        ),
        (
            "DROP POLICY p ON nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER POLICY p ON t RENAME TO p2;",
            "policy \"p2\" for table \"t\" already exists",
        ),
        (
            "ALTER POLICY nosuch ON t RENAME TO z;",
            "policy \"nosuch\" for table \"t\" does not exist",
        ),
        (
            "ALTER POLICY p ON t RENAME TO q; DROP POLICY p ON t;",
            "policy \"p\" for table \"t\" does not exist",
        ),
        (
            "DROP POLICY p ON t; ALTER POLICY p ON t USING (true);",
            "policy \"p\" for table \"t\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE POLICY p3 ON t USING (t.a > 0) WITH CHECK ((SELECT count(*) FROM t) > 0);
             CREATE POLICY p4 ON t USING (null);
             DROP POLICY IF EXISTS nosuch ON t;
             DROP POLICY IF EXISTS nosuch ON nosuch;
             ALTER POLICY p ON t RENAME TO q;
             ALTER POLICY q ON t USING (a < 0);
             DROP POLICY q ON t;
             CREATE POLICY p ON t USING (true);",
        ),
    ]);
}

#[test]
fn ddl_expression_kinds_reject_aggregates_windows_and_subqueries() {
    // PG 18 transformExpr with EXPR_KIND_CHECK_CONSTRAINT / DOMAIN_CHECK /
    // INDEX_EXPRESSION / INDEX_PREDICATE / GENERATED_COLUMN: aggregates and
    // window functions fail in check_agglevels_and_constraints /
    // transformWindowFuncCall, sublinks in transformSubLink.
    let setup = "CREATE TABLE t (a int); CREATE DOMAIN dd AS int;";
    for (stmt, msg) in [
        (
            "CREATE TABLE c1 (a int CHECK (sum(a) > 0));",
            "aggregate functions are not allowed in check constraints",
        ),
        (
            "CREATE TABLE c2 (a int CHECK (row_number() over () > 0));",
            "window functions are not allowed in check constraints",
        ),
        (
            "CREATE TABLE c3 (a int CHECK ((select 1) > 0));",
            "cannot use subquery in check constraint",
        ),
        (
            "CREATE TABLE c4 (a int, CHECK (max(a) > 0));",
            "aggregate functions are not allowed in check constraints",
        ),
        (
            "ALTER TABLE t ADD CHECK (sum(a) > 0);",
            "aggregate functions are not allowed in check constraints",
        ),
        (
            "ALTER TABLE t ADD CHECK ((select true));",
            "cannot use subquery in check constraint",
        ),
        (
            "CREATE DOMAIN d AS int CHECK (sum(VALUE) > 0);",
            "aggregate functions are not allowed in check constraints",
        ),
        (
            "CREATE DOMAIN d2 AS int CHECK ((select true));",
            "cannot use subquery in check constraint",
        ),
        (
            "ALTER DOMAIN dd ADD CHECK (sum(VALUE) > 0);",
            "aggregate functions are not allowed in check constraints",
        ),
        (
            "CREATE INDEX ON t ((sum(a)));",
            "aggregate functions are not allowed in index expressions",
        ),
        (
            "CREATE INDEX ON t ((row_number() over ()));",
            "window functions are not allowed in index expressions",
        ),
        (
            "CREATE INDEX ON t (((select 1)));",
            "cannot use subquery in index expression",
        ),
        (
            "CREATE INDEX ON t (a) WHERE sum(a) > 0;",
            "aggregate functions are not allowed in index predicates",
        ),
        (
            "CREATE INDEX ON t (a) WHERE (select true);",
            "cannot use subquery in index predicate",
        ),
        (
            "CREATE TABLE g1 (a int, b int GENERATED ALWAYS AS (sum(a)) STORED);",
            "aggregate functions are not allowed in column generation expressions",
        ),
        (
            "CREATE TABLE g2 (a int, b bigint GENERATED ALWAYS AS (row_number() over ()) STORED);",
            "window functions are not allowed in column generation expressions",
        ),
        (
            "CREATE TABLE g3 (a int, b int GENERATED ALWAYS AS ((select 1)) STORED);",
            "cannot use subquery in column generation expression",
        ),
        (
            "CREATE POLICY p ON t USING (row_number() over () > 0);",
            "window functions are not allowed in policy expressions",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
}

#[test]
fn builtin_sql_functions_count_by_their_inlined_body() {
    // PG 18: `text || int` runs the STABLE LANGUAGE sql textanycat, which
    // expression_planner inlines to `$1 || $2::text` before
    // CheckMutability, so generation and index expressions accept it.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (
             a int,
             b text GENERATED ALWAYS AS (a::text || 1) STORED,
             c text GENERATED ALWAYS AS (quote_literal(a)) STORED
         );
         CREATE INDEX ON t ((1 || a::text));",
    )]);
}

#[test]
fn index_expressions_and_predicates_are_analyzed() {
    // PG 18 DefineIndex: the predicate is transformed as a boolean WHERE
    // over the table's row, then CheckPredicate requires it to be
    // IMMUTABLE; index expressions resolve their columns.
    let setup = "CREATE TABLE t (a int);";
    for (stmt, msg) in [
        (
            "CREATE INDEX ON t (a) WHERE a;",
            "argument of WHERE must be type boolean, not type integer",
        ),
        (
            "CREATE INDEX ON t (a) WHERE nosuch > 0;",
            "column \"nosuch\" does not exist",
        ),
        (
            "CREATE INDEX ON t (a) WHERE random() > 0.5;",
            "functions in index predicate must be marked IMMUTABLE",
        ),
        (
            "CREATE INDEX ON t (a) WHERE now() > '2020-01-01';",
            "functions in index predicate must be marked IMMUTABLE",
        ),
        (
            "CREATE INDEX ON t ((a + nosuch));",
            "column \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE INDEX ON t (a) WHERE a > 0;
             CREATE INDEX ON t ((a::text || 1));
             CREATE INDEX ON t (a) WHERE t.a IS NOT NULL;",
        ),
    ]);
}
