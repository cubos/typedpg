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

#[test]
fn alter_column_settings_are_validated() {
    // PG 18 ATExecSetStatistics / ATExecSetStorage / ATExecSetCompression /
    // ATExecSetOptions.
    let setup = "CREATE TABLE t (a int, b text, c int[]);
                 CREATE VIEW v AS SELECT 1 AS x;
                 CREATE INDEX ti ON t ((a + 1), b);";
    for (stmt, msg) in [
        (
            "ALTER TABLE t ALTER COLUMN nosuch SET STATISTICS 100;",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a SET STATISTICS -5;",
            "statistics target -5 is too low",
        ),
        (
            "ALTER TABLE t ALTER COLUMN ctid SET STATISTICS 5;",
            "cannot alter system column \"ctid\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN 1 SET STATISTICS 5;",
            "cannot refer to non-index column by number",
        ),
        (
            "ALTER INDEX ti ALTER COLUMN 2 SET STATISTICS 5;",
            "cannot alter statistics on non-expression column \"b\" of index \"ti\"",
        ),
        (
            "ALTER INDEX ti ALTER COLUMN 3 SET STATISTICS 5;",
            "column number 3 of relation \"ti\" does not exist",
        ),
        (
            "ALTER TABLE v ALTER COLUMN x SET STATISTICS 5;",
            "ALTER action ALTER COLUMN ... SET STATISTICS cannot be performed on relation \"v\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN nosuch SET STORAGE EXTERNAL;",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a SET STORAGE EXTERNAL;",
            "column data type integer can only have storage PLAIN",
        ),
        (
            "ALTER TABLE t ALTER COLUMN b SET STORAGE nosuch;",
            "invalid storage type \"nosuch\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN nosuch SET COMPRESSION pglz;",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a SET COMPRESSION pglz;",
            "column data type integer does not support compression",
        ),
        (
            "ALTER TABLE t ALTER COLUMN b SET COMPRESSION nosuch;",
            "invalid compression method \"nosuch\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN nosuch SET (n_distinct = 1);",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a SET (nosuchopt = 1);",
            "unrecognized parameter \"nosuchopt\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a SET (n_distinct = -2);",
            "value -2 out of bounds for option \"n_distinct\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a SET (n_distinct = 'abc');",
            "invalid value for floating point option \"n_distinct\": abc",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a SET (n_distinct);",
            "invalid value for floating point option \"n_distinct\": true",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a SET (x.y = 1);",
            "unrecognized parameter namespace \"x\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a SET (n_distinct = 5, n_distinct = 6);",
            "parameter \"n_distinct\" specified more than once",
        ),
        (
            "ALTER TABLE t ALTER COLUMN nosuch RESET (n_distinct);",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN a SET STATISTICS 100000;
             ALTER TABLE t ALTER COLUMN a SET STATISTICS DEFAULT;
             ALTER TABLE t ALTER COLUMN a SET STATISTICS -1;
             ALTER TABLE t ALTER COLUMN a SET STORAGE PLAIN;
             ALTER TABLE t ALTER COLUMN a SET STORAGE DEFAULT;
             ALTER TABLE t ALTER COLUMN b SET STORAGE EXTERNAL;
             ALTER TABLE t ALTER COLUMN c SET STORAGE MAIN;
             ALTER TABLE t ALTER COLUMN a SET COMPRESSION default;
             ALTER TABLE t ALTER COLUMN b SET COMPRESSION lz4;
             ALTER TABLE t ALTER COLUMN a SET (n_distinct = -1, n_distinct_inherited = 5);
             ALTER TABLE t ALTER COLUMN a RESET (nosuch);
             ALTER INDEX ti ALTER COLUMN 1 SET STATISTICS 5;",
        ),
    ]);
}

#[test]
fn alter_table_object_references_are_resolved() {
    // PG 18 check_index_is_clusterable / ATExecReplicaIdentity /
    // ATExecAlterConstraint / ATExecValidateConstraint /
    // EnableDisableTrigger.
    let setup = "CREATE TABLE r (id int PRIMARY KEY);
                 CREATE TABLE t (a int, b int REFERENCES r, c int, d int NOT NULL,
                                 CONSTRAINT ck CHECK (a > 0), CONSTRAINT uq UNIQUE (c));
                 CREATE INDEX t_a_idx ON t (a);
                 CREATE UNIQUE INDEX t_part ON t (d) WHERE d > 0;
                 CREATE UNIQUE INDEX t_expr ON t ((d + 1));
                 CREATE UNIQUE INDEX t_c2 ON t (c);
                 CREATE UNIQUE INDEX t_d ON t (d);
                 CREATE INDEX r_idx ON r (id);
                 CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
                 CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();";
    for (stmt, msg) in [
        (
            "ALTER TABLE t CLUSTER ON nosuch_idx;",
            "index \"nosuch_idx\" for table \"t\" does not exist",
        ),
        (
            "ALTER TABLE t CLUSTER ON r_idx;",
            "\"r_idx\" is not an index for table \"t\"",
        ),
        (
            "ALTER TABLE t CLUSTER ON t_part;",
            "cannot cluster on partial index \"t_part\"",
        ),
        (
            "ALTER TABLE t REPLICA IDENTITY USING INDEX nosuch_idx;",
            "index \"nosuch_idx\" for table \"t\" does not exist",
        ),
        (
            "ALTER TABLE t REPLICA IDENTITY USING INDEX t_a_idx;",
            "cannot use non-unique index \"t_a_idx\" as replica identity",
        ),
        (
            "ALTER TABLE t REPLICA IDENTITY USING INDEX t_expr;",
            "cannot use expression index \"t_expr\" as replica identity",
        ),
        (
            "ALTER TABLE t REPLICA IDENTITY USING INDEX t_part;",
            "cannot use partial index \"t_part\" as replica identity",
        ),
        (
            "ALTER TABLE t REPLICA IDENTITY USING INDEX t_c2;",
            "index \"t_c2\" cannot be used as replica identity because column \"c\" is nullable",
        ),
        (
            "ALTER TABLE t REPLICA IDENTITY USING INDEX uq;",
            "index \"uq\" cannot be used as replica identity because column \"c\" is nullable",
        ),
        (
            "ALTER TABLE t ALTER CONSTRAINT nosuch DEFERRABLE;",
            "constraint \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t ALTER CONSTRAINT ck DEFERRABLE;",
            "constraint \"ck\" of relation \"t\" is not a foreign key constraint",
        ),
        (
            "ALTER TABLE t VALIDATE CONSTRAINT nosuch;",
            "constraint \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t VALIDATE CONSTRAINT uq;",
            "cannot validate constraint \"uq\" of relation \"t\"",
        ),
        (
            "ALTER TABLE t ENABLE TRIGGER nosuch;",
            "trigger \"nosuch\" for table \"t\" does not exist",
        ),
        (
            "ALTER TABLE t DISABLE TRIGGER nosuch;",
            "trigger \"nosuch\" for table \"t\" does not exist",
        ),
        (
            "ALTER TABLE t ENABLE REPLICA TRIGGER nosuch;",
            "trigger \"nosuch\" for table \"t\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE t CLUSTER ON t_a_idx;
             ALTER TABLE t REPLICA IDENTITY USING INDEX t_d;
             ALTER TABLE t REPLICA IDENTITY FULL;
             ALTER TABLE t ALTER CONSTRAINT t_b_fkey DEFERRABLE;
             ALTER TABLE t VALIDATE CONSTRAINT ck;
             ALTER TABLE t VALIDATE CONSTRAINT t_b_fkey;
             ALTER TABLE t ENABLE TRIGGER ALL;
             ALTER TABLE t DISABLE TRIGGER USER;
             ALTER TABLE t DISABLE TRIGGER tr;
             ALTER TABLE t ENABLE ALWAYS TRIGGER tr;",
        ),
    ]);
}

#[test]
fn check_constraints_are_inherited() {
    // PG 18 MergeCheckConstraint / MergeWithExistingConstraint /
    // ATAddCheckNNConstraint / dropconstraint_internal /
    // rename_constraint_internal.
    let setup = "CREATE TABLE p (a int CONSTRAINT pc CHECK (a > 0));
                 CREATE TABLE p2 (a int CONSTRAINT pc CHECK (a > 1));
                 CREATE TABLE p3 (a int CONSTRAINT pc CHECK (a > 0));
                 CREATE TABLE c () INHERITS (p);
                 CREATE TABLE c2 () INHERITS (p, p3);";
    for (stmt, msg) in [
        (
            "CREATE TABLE x () INHERITS (p, p2);",
            "check constraint name \"pc\" appears multiple times but with different expressions",
        ),
        (
            "CREATE TABLE x (CONSTRAINT pc CHECK (a > 5)) INHERITS (p);",
            "constraint \"pc\" for relation \"x\" already exists",
        ),
        (
            "CREATE TABLE x (CONSTRAINT pc CHECK (a > 0) NO INHERIT) INHERITS (p);",
            "constraint \"pc\" conflicts with inherited constraint on relation \"x\"",
        ),
        (
            "ALTER TABLE c DROP CONSTRAINT pc;",
            "cannot drop inherited constraint \"pc\" of relation \"c\"",
        ),
        (
            "ALTER TABLE c ADD CONSTRAINT pc CHECK (a > 5);",
            "constraint \"pc\" for relation \"c\" already exists",
        ),
        (
            "ALTER TABLE p ADD CONSTRAINT pc2 CHECK (a > 1); ALTER TABLE c DROP CONSTRAINT pc2;",
            "cannot drop inherited constraint \"pc2\" of relation \"c\"",
        ),
        (
            "ALTER TABLE ONLY p ADD CONSTRAINT q CHECK (a < 100);",
            "constraint must be added to child tables too",
        ),
        (
            "ALTER TABLE c ADD CONSTRAINT z CHECK (a < 5); ALTER TABLE p ADD CONSTRAINT z CHECK (a < 6);",
            "constraint \"z\" for relation \"c\" already exists",
        ),
        (
            "ALTER TABLE p ADD CONSTRAINT k CHECK (a < 9); ALTER TABLE p ADD CONSTRAINT k CHECK (a < 9);",
            "constraint \"k\" for relation \"p\" already exists",
        ),
        (
            "ALTER TABLE c ADD CONSTRAINT z CHECK (a < 5) NO INHERIT; ALTER TABLE p ADD CONSTRAINT z CHECK (a < 5);",
            "constraint \"z\" conflicts with non-inherited constraint on relation \"c\"",
        ),
        (
            "ALTER TABLE c RENAME CONSTRAINT pc TO pz;",
            "cannot rename inherited constraint \"pc\"",
        ),
        (
            "ALTER TABLE ONLY p RENAME CONSTRAINT pc TO pz;",
            "inherited constraint \"pc\" must be renamed in child tables too",
        ),
        (
            // c2 inherits pc from p3 as well.
            "ALTER TABLE p RENAME CONSTRAINT pc TO pz;",
            "cannot rename inherited constraint \"pc\"",
        ),
        (
            "ALTER TABLE p ADD CONSTRAINT y CHECK (a < 7);
             ALTER TABLE p RENAME CONSTRAINT y TO w; ALTER TABLE c DROP CONSTRAINT w;",
            "cannot drop inherited constraint \"w\" of relation \"c\"",
        ),
        (
            "ALTER TABLE p DROP CONSTRAINT pc; ALTER TABLE c2 DROP CONSTRAINT pc;",
            "cannot drop inherited constraint \"pc\" of relation \"c2\"",
        ),
        (
            "ALTER TABLE p DROP CONSTRAINT pc; ALTER TABLE c DROP CONSTRAINT pc;",
            "constraint \"pc\" of relation \"c\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE c4 (CONSTRAINT pc CHECK (a>0)) INHERITS (p);
             ALTER TABLE ONLY p ADD CONSTRAINT q CHECK (a < 100) NO INHERIT;
             ALTER TABLE c ADD CONSTRAINT z CHECK (a < 5);
             ALTER TABLE p ADD CONSTRAINT z CHECK (a < 5);
             ALTER TABLE c VALIDATE CONSTRAINT pc;
             ALTER TABLE ONLY p DROP CONSTRAINT z;
             ALTER TABLE c DROP CONSTRAINT z;
             ALTER TABLE c4 DROP CONSTRAINT z;
             ALTER TABLE p ADD CONSTRAINT y CHECK (a < 7);
             ALTER TABLE p RENAME CONSTRAINT y TO w;
             ALTER TABLE p DROP CONSTRAINT w;
             ALTER TABLE p DROP CONSTRAINT pc;
             ALTER TABLE c4 DROP CONSTRAINT pc;",
        ),
    ]);
    // A CHECK added with a new column reaches the children.
    let err = try_apply(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE p ADD COLUMN b int CONSTRAINT bc CHECK (b > 0);
             ALTER TABLE c DROP CONSTRAINT bc;",
        ),
    ])
    .expect_err("ADD COLUMN CHECK");
    assert!(
        err.to_string()
            .starts_with("cannot drop inherited constraint \"bc\" of relation \"c\""),
        "got: {err}"
    );
}

#[test]
fn typed_tables_follow_their_type() {
    // PG 18 check_of_type / ATExecAddOf / ATExecDropOf /
    // ATTypedTableRecursion / find_typed_table_dependencies.
    let setup = "CREATE TYPE ct AS (a int, b text);
                 CREATE TABLE t9 OF ct;
                 CREATE TABLE p (a int);
                 CREATE TABLE t1 (a int, b text);
                 CREATE TABLE t2 (b text, a int);
                 CREATE TABLE t3 (a bigint, b text);
                 CREATE TABLE t4 (a int);
                 CREATE TABLE t5 (a int, b text, c int);
                 CREATE TABLE t6 (a int, b text COLLATE \"C\");
                 CREATE TABLE t7 (a int, b text) INHERITS (p);";
    for (stmt, msg) in [
        (
            "ALTER TABLE t9 DROP COLUMN a;",
            "cannot drop column from typed table",
        ),
        (
            "ALTER TABLE t9 ADD COLUMN z int;",
            "cannot add column to typed table",
        ),
        (
            "ALTER TABLE t9 ALTER COLUMN a TYPE bigint;",
            "cannot alter column type of typed table",
        ),
        (
            "ALTER TABLE t9 RENAME COLUMN a TO z;",
            "cannot rename column of typed table",
        ),
        (
            "ALTER TYPE ct ADD ATTRIBUTE c int;",
            "cannot alter type \"ct\" because it is the type of a typed table",
        ),
        (
            "ALTER TYPE ct DROP ATTRIBUTE b;",
            "cannot alter type \"ct\" because it is the type of a typed table",
        ),
        (
            "ALTER TYPE ct RENAME ATTRIBUTE a TO z;",
            "cannot alter type \"ct\" because it is the type of a typed table",
        ),
        (
            "DROP TYPE ct;",
            "cannot drop type ct because other objects depend on it",
        ),
        (
            "CREATE TABLE x OF p;",
            "type p is the row type of another table",
        ),
        (
            "ALTER TABLE t1 OF int4;",
            "type integer is not a composite type",
        ),
        (
            "ALTER TABLE t1 OF p;",
            "type p is the row type of another table",
        ),
        (
            "ALTER TABLE t2 OF ct;",
            "table has column \"b\" where type requires \"a\"",
        ),
        (
            "ALTER TABLE t3 OF ct;",
            "table \"t3\" has different type for column \"a\"",
        ),
        ("ALTER TABLE t4 OF ct;", "table is missing column \"b\""),
        ("ALTER TABLE t5 OF ct;", "table has extra column \"c\""),
        (
            "ALTER TABLE t6 OF ct;",
            "table \"t6\" has different type for column \"b\"",
        ),
        ("ALTER TABLE t7 OF ct;", "typed tables cannot inherit"),
        ("ALTER TABLE t1 NOT OF;", "\"t1\" is not a typed table"),
        (
            "ALTER TABLE t1 OF ct; ALTER TABLE t1 ADD COLUMN z int;",
            "cannot add column to typed table",
        ),
        (
            "ALTER TYPE ct ADD ATTRIBUTE c int CASCADE; ALTER TABLE t9 ADD COLUMN c int;",
            "cannot add column to typed table",
        ),
        (
            "ALTER TYPE ct RENAME ATTRIBUTE a TO z CASCADE; ALTER TABLE t9 DROP COLUMN a;",
            "cannot drop column from typed table",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE t1 OF ct;
             ALTER TABLE t1 NOT OF;
             ALTER TABLE t1 ADD COLUMN extra int;
             ALTER TYPE ct ADD ATTRIBUTE c int CASCADE;
             ALTER TYPE ct RENAME ATTRIBUTE a TO z CASCADE;
             ALTER TYPE ct DROP ATTRIBUTE b CASCADE;
             ALTER TYPE ct ALTER ATTRIBUTE c TYPE bigint CASCADE;
             CREATE TABLE t10 OF ct;
             DROP TYPE ct CASCADE;",
        ),
    ]);
    let seed = db.to_seed();
    assert!(
        seed.pg_class
            .iter()
            .all(|c| c.relname != "t9" && c.relname != "t10")
    );
    // The CASCADE changes reached the typed table before it was dropped.
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TYPE ct ADD ATTRIBUTE c int CASCADE;
             ALTER TYPE ct RENAME ATTRIBUTE a TO z CASCADE;
             ALTER TYPE ct DROP ATTRIBUTE b CASCADE;
             ALTER TYPE ct ALTER ATTRIBUTE c TYPE bigint CASCADE;",
        ),
    ]);
    let seed = db.to_seed();
    let t9 = seed
        .pg_class
        .iter()
        .find(|c| c.relname == "t9")
        .unwrap()
        .oid;
    let cols: Vec<(String, u32)> = seed
        .pg_attribute
        .iter()
        .filter(|a| a.attrelid == t9)
        .map(|a| (a.attname.clone(), a.atttypid.get()))
        .collect();
    assert_eq!(cols, vec![("z".to_owned(), 23), ("c".to_owned(), 20)]);
}

#[test]
fn drop_column_drops_the_check_constraints_that_read_it() {
    // PG 18: a CHECK constraint's conkey lists the columns its expression
    // reads, and dropping one of them drops the constraint.
    let setup = "CREATE TABLE t (a int CHECK (a > 1), b int CHECK (a < b),
                                 CONSTRAINT one CHECK (a > 0), CONSTRAINT two CHECK (a > b),
                                 CONSTRAINT k CHECK (b > 0));
                 ALTER TABLE t ADD CONSTRAINT three CHECK (a < 100);
                 CREATE TABLE p (x int);
                 CREATE TABLE c () INHERITS (p);
                 ALTER TABLE p ADD COLUMN y int CONSTRAINT yc CHECK (y > 0);
                 ALTER TABLE t DROP COLUMN a;
                 ALTER TABLE p DROP COLUMN y;";
    for (stmt, msg) in [
        (
            "ALTER TABLE t DROP CONSTRAINT one;",
            "constraint \"one\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t DROP CONSTRAINT two;",
            "constraint \"two\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t DROP CONSTRAINT three;",
            "constraint \"three\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t DROP CONSTRAINT t_check;",
            "constraint \"t_check\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE c DROP CONSTRAINT yc;",
            "constraint \"yc\" of relation \"c\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        ("0002.sql", "ALTER TABLE t DROP CONSTRAINT k;"),
    ]);
}

#[test]
fn alter_table_inherit_and_no_inherit() {
    // PG 18 ATPrepAddInherit / ATExecAddInherit / CreateInheritance /
    // MergeAttributesIntoExisting / MergeConstraintsIntoExisting /
    // RemoveInheritance.
    let setup = "CREATE TABLE p (a int NOT NULL, b text, CONSTRAINT pc CHECK (a > 0));
                 CREATE VIEW v AS SELECT 1 AS a;
                 CREATE TABLE c1 (x int);
                 CREATE TABLE c2 (a bigint, b text);
                 CREATE TABLE c3 (a int, b text);
                 CREATE TABLE c4 (a int NOT NULL, b text);
                 CREATE TABLE c5 (a int NOT NULL, b text, CONSTRAINT pc CHECK (a > 1));
                 CREATE TABLE c6 (a int NOT NULL, b text COLLATE \"C\", CONSTRAINT pc CHECK (a > 0));
                 CREATE TABLE c7 (a int NOT NULL, b text, extra int, CONSTRAINT pc CHECK (a > 0));
                 CREATE TABLE c8 (a int NOT NULL, b text, CONSTRAINT pc CHECK (a > 0) NO INHERIT);
                 CREATE TABLE c9 (a int NOT NULL, b text GENERATED ALWAYS AS ('x') STORED,
                                  CONSTRAINT pc CHECK (a > 0));
                 CREATE TABLE pt (a int NOT NULL, b text) PARTITION BY LIST (a);
                 CREATE TABLE part PARTITION OF pt FOR VALUES IN (1);
                 CREATE TYPE ct AS (a int, b text);
                 CREATE TABLE tt OF ct;";
    for (stmt, msg) in [
        (
            "ALTER TABLE c1 INHERIT p;",
            "child table is missing column \"a\"",
        ),
        (
            "ALTER TABLE c2 INHERIT p;",
            "child table \"c2\" has different type for column \"a\"",
        ),
        (
            "ALTER TABLE c3 INHERIT p;",
            "column \"a\" in child table \"c3\" must be marked NOT NULL",
        ),
        (
            "ALTER TABLE c4 INHERIT p;",
            "child table is missing constraint \"pc\"",
        ),
        (
            "ALTER TABLE c5 INHERIT p;",
            "child table \"c5\" has different definition for check constraint \"pc\"",
        ),
        (
            "ALTER TABLE c6 INHERIT p;",
            "child table \"c6\" has different collation for column \"b\"",
        ),
        (
            "ALTER TABLE c8 INHERIT p;",
            "constraint \"pc\" conflicts with non-inherited constraint on child table \"c8\"",
        ),
        (
            "ALTER TABLE c9 INHERIT p;",
            "column \"b\" in child table must not be a generated column",
        ),
        (
            "ALTER TABLE c7 INHERIT p; ALTER TABLE c7 INHERIT p;",
            "relation \"p\" would be inherited from more than once",
        ),
        (
            "ALTER TABLE c7 INHERIT p; ALTER TABLE p INHERIT c7;",
            "circular inheritance not allowed",
        ),
        (
            "ALTER TABLE c7 INHERIT c7;",
            "circular inheritance not allowed",
        ),
        (
            "ALTER TABLE c7 INHERIT v;",
            "ALTER action INHERIT cannot be performed on relation \"v\"",
        ),
        (
            "ALTER TABLE tt INHERIT p;",
            "cannot change inheritance of typed table",
        ),
        (
            "ALTER TABLE part INHERIT p;",
            "cannot change inheritance of a partition",
        ),
        (
            "ALTER TABLE part NO INHERIT pt;",
            "cannot change inheritance of a partition",
        ),
        (
            "ALTER TABLE c1 INHERIT pt;",
            "cannot inherit from partitioned table \"pt\"",
        ),
        (
            "ALTER TABLE pt INHERIT c1;",
            "cannot change inheritance of partitioned table",
        ),
        (
            "ALTER TABLE c7 INHERIT part;",
            "cannot inherit from a partition",
        ),
        (
            "ALTER TABLE c7 INHERIT nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE c7 NO INHERIT nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE c7 NO INHERIT p;",
            "relation \"p\" is not a parent of relation \"c7\"",
        ),
        (
            "ALTER TABLE c7 INHERIT p; ALTER TABLE c7 DROP COLUMN a;",
            "cannot drop inherited column \"a\"",
        ),
        (
            "ALTER TABLE c7 INHERIT p; ALTER TABLE c7 DROP CONSTRAINT pc;",
            "cannot drop inherited constraint \"pc\" of relation \"c7\"",
        ),
        (
            "ALTER TABLE c7 INHERIT p; ALTER TABLE ONLY p ADD CONSTRAINT q CHECK (a < 9);",
            "constraint must be added to child tables too",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE c7 INHERIT p;
             ALTER TABLE p ADD COLUMN z int;
             ALTER TABLE c7 NO INHERIT p;
             ALTER TABLE c7 DROP CONSTRAINT pc;
             ALTER TABLE c7 DROP COLUMN a;
             ALTER TABLE c7 DROP COLUMN z;
             ALTER TABLE ONLY p ADD CONSTRAINT q CHECK (a < 9);",
        ),
    ]);
}

#[test]
fn attach_and_detach_partition() {
    // PG 18 ATExecAttachPartition / ATExecDetachPartition.
    let setup = "CREATE TABLE pt (a int NOT NULL, b text, CONSTRAINT pc CHECK (a > 0))
                     PARTITION BY LIST (a);
                 CREATE TABLE x1 (a int NOT NULL, b text, CONSTRAINT pc CHECK (a > 0));
                 CREATE TABLE x2 (a int, b text);
                 CREATE TABLE x3 (a int NOT NULL, b text, c int);
                 CREATE TABLE x4 (a int NOT NULL);
                 CREATE TABLE x5 (a bigint NOT NULL, b text);
                 CREATE TABLE x6 (a int NOT NULL, b text);
                 CREATE TABLE plain (a int);
                 CREATE TABLE ch (a int NOT NULL, b text, CONSTRAINT pc CHECK (a > 0));
                 CREATE TABLE ih () INHERITS (ch);
                 CREATE TYPE ct AS (a int, b text);
                 CREATE TABLE tt OF ct;
                 CREATE VIEW v AS SELECT 1 AS a, 'x'::text AS b;";
    for (stmt, msg) in [
        (
            "ALTER TABLE pt ATTACH PARTITION x2 FOR VALUES IN (2);",
            "column \"a\" in child table \"x2\" must be marked NOT NULL",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION x3 FOR VALUES IN (3);",
            "table \"x3\" contains column \"c\" not found in parent \"pt\"",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION x4 FOR VALUES IN (4);",
            "child table is missing column \"b\"",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION x5 FOR VALUES IN (5);",
            "child table \"x5\" has different type for column \"a\"",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION x6 FOR VALUES IN (6);",
            "child table is missing constraint \"pc\"",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION nosuch FOR VALUES IN (7);",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE plain ATTACH PARTITION x1 FOR VALUES IN (1);",
            "ALTER action ATTACH PARTITION cannot be performed on relation \"plain\"",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION v FOR VALUES IN (1);",
            "ALTER action ATTACH PARTITION cannot be performed on relation \"v\"",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION x1 FOR VALUES IN (1);
             ALTER TABLE pt ATTACH PARTITION x1 FOR VALUES IN (8);",
            "\"x1\" is already a partition",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION ih FOR VALUES IN (1);",
            "cannot attach inheritance child as partition",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION ch FOR VALUES IN (1);",
            "cannot attach inheritance parent as partition",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION tt FOR VALUES IN (1);",
            "cannot attach a typed table as partition",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION x1 FOR VALUES IN (1); ALTER TABLE x1 DROP CONSTRAINT pc;",
            "cannot drop inherited constraint \"pc\" of relation \"x1\"",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION x1 FOR VALUES IN (1); ALTER TABLE x1 DROP COLUMN b;",
            "cannot drop inherited column \"b\"",
        ),
        (
            "ALTER TABLE pt ATTACH PARTITION x1 FOR VALUES IN (1); ALTER TABLE x1 RENAME COLUMN a TO z;",
            "cannot rename inherited column \"a\"",
        ),
        (
            "ALTER TABLE pt DETACH PARTITION x6;",
            "relation \"x6\" is not a partition of relation \"pt\"",
        ),
        (
            "ALTER TABLE pt DETACH PARTITION nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE plain DETACH PARTITION x1;",
            "ALTER action DETACH PARTITION cannot be performed on relation \"plain\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE pt ATTACH PARTITION x1 FOR VALUES IN (1);
             ALTER TABLE pt DETACH PARTITION x1;
             ALTER TABLE x1 DROP CONSTRAINT pc;
             ALTER TABLE x1 DROP COLUMN b;",
        ),
    ]);
}

#[test]
fn partition_bounds_are_validated() {
    // PG 18 transformPartitionBound / transformPartitionBoundValue /
    // validateInfiniteBounds / check_new_partition_bound.
    let setup = "CREATE TABLE pl (a int, b text) PARTITION BY LIST (a);
                 CREATE TABLE pr (a int, b int) PARTITION BY RANGE (a, b);
                 CREATE TABLE ph (a int) PARTITION BY HASH (a);
                 CREATE TABLE pd (d date) PARTITION BY RANGE (d);
                 CREATE TABLE ps (s text) PARTITION BY LIST (s);
                 CREATE TABLE l3 PARTITION OF pl FOR VALUES IN (1, 2);
                 CREATE TABLE l5 PARTITION OF pl DEFAULT;
                 CREATE TABLE l7 PARTITION OF pl FOR VALUES IN (NULL);
                 CREATE TABLE l9 PARTITION OF pl FOR VALUES IN (1 + 4);
                 CREATE TABLE r4 PARTITION OF pr FOR VALUES FROM (1, 1) TO (5, 5);
                 CREATE TABLE r7 PARTITION OF pr FOR VALUES FROM (5, 5) TO (MAXVALUE, MAXVALUE);
                 CREATE TABLE h3 PARTITION OF ph FOR VALUES WITH (MODULUS 4, REMAINDER 1);
                 CREATE TABLE d1 PARTITION OF pd FOR VALUES FROM ('2024-01-01') TO ('2024-02-01');
                 CREATE TABLE s1 PARTITION OF ps FOR VALUES IN ('x', 'x');
                 CREATE TABLE free (a int, b text);";
    for (stmt, msg) in [
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES FROM (1) TO (2);",
            "invalid bound specification for a list partition",
        ),
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES IN ('x');",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES IN (2);",
            "partition \"x\" would overlap partition \"l3\"",
        ),
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES IN (5);",
            "partition \"x\" would overlap partition \"l9\"",
        ),
        (
            "CREATE TABLE x PARTITION OF pl DEFAULT;",
            "partition \"x\" conflicts with existing default partition \"l5\"",
        ),
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES IN (NULL);",
            "partition \"x\" would overlap partition \"l7\"",
        ),
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES IN ((SELECT 1));",
            "cannot use subquery in partition bound",
        ),
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES IN (sum(1));",
            "aggregate functions are not allowed in partition bound",
        ),
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES IN (b);",
            "cannot use column reference in partition bound expression",
        ),
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES IN (MINVALUE);",
            "cannot use column reference in partition bound expression",
        ),
        (
            "CREATE TABLE x PARTITION OF pl FOR VALUES IN (now());",
            "specified value cannot be cast to type integer for column \"a\"",
        ),
        (
            "CREATE TABLE x PARTITION OF pr FOR VALUES IN (1);",
            "invalid bound specification for a range partition",
        ),
        (
            "CREATE TABLE x PARTITION OF pr FOR VALUES FROM (1) TO (2);",
            "FROM must specify exactly one value per partitioning column",
        ),
        (
            "CREATE TABLE x PARTITION OF pr FOR VALUES FROM (1, 1) TO (2);",
            "TO must specify exactly one value per partitioning column",
        ),
        (
            "CREATE TABLE x PARTITION OF pr FOR VALUES FROM (7, 1) TO (7, 1);",
            "empty range bound specified for partition \"x\"",
        ),
        (
            "CREATE TABLE x PARTITION OF pr FOR VALUES FROM (4, 0) TO (9, 9);",
            "partition \"x\" would overlap partition \"r4\"",
        ),
        (
            "CREATE TABLE x PARTITION OF pr FOR VALUES FROM (MINVALUE, 0) TO (1, 0);",
            "every bound following MINVALUE must also be MINVALUE",
        ),
        (
            "CREATE TABLE x PARTITION OF pr FOR VALUES FROM (0, 0) TO (MAXVALUE, 1);",
            "every bound following MAXVALUE must also be MAXVALUE",
        ),
        (
            "CREATE TABLE x PARTITION OF pr FOR VALUES FROM (NULL, 1) TO (1, 1);",
            "cannot specify NULL in range bound",
        ),
        (
            "CREATE TABLE x PARTITION OF pr FOR VALUES FROM ('x', 1) TO (9, 9);",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "CREATE TABLE x PARTITION OF pd FOR VALUES FROM ('2024-01-15') TO ('2024-03-01');",
            "partition \"x\" would overlap partition \"d1\"",
        ),
        (
            "CREATE TABLE x PARTITION OF ph FOR VALUES WITH (MODULUS 4, REMAINDER 5);",
            "remainder for hash partition must be less than modulus",
        ),
        (
            "CREATE TABLE x PARTITION OF ph FOR VALUES WITH (MODULUS 0, REMAINDER 0);",
            "modulus for hash partition must be an integer value greater than zero",
        ),
        (
            "CREATE TABLE x PARTITION OF ph FOR VALUES WITH (MODULUS 4, REMAINDER 1);",
            "partition \"x\" would overlap partition \"h3\"",
        ),
        (
            "CREATE TABLE x PARTITION OF ph FOR VALUES WITH (MODULUS 3, REMAINDER 1);",
            "every hash partition modulus must be a factor of the next larger modulus",
        ),
        (
            "CREATE TABLE x PARTITION OF ph FOR VALUES WITH (MODULUS 8, REMAINDER 5);",
            "partition \"x\" would overlap partition \"h3\"",
        ),
        (
            "CREATE TABLE x PARTITION OF ph DEFAULT;",
            "a hash-partitioned table may not have a default partition",
        ),
        (
            "CREATE TABLE x PARTITION OF ph FOR VALUES IN (1);",
            "invalid bound specification for a hash partition",
        ),
        (
            "CREATE TABLE x PARTITION OF ps FOR VALUES IN ('x');",
            "partition \"x\" would overlap partition \"s1\"",
        ),
        (
            "ALTER TABLE pl ATTACH PARTITION free FOR VALUES IN (2);",
            "partition \"free\" would overlap partition \"l3\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE a1 PARTITION OF pl FOR VALUES IN (3, 3, 4);
             CREATE TABLE a2 PARTITION OF pr FOR VALUES FROM (MINVALUE, MINVALUE) TO (1, 1);
             CREATE TABLE a3 PARTITION OF pd FOR VALUES FROM ('2024-02-01') TO ('2024-03-01');
             CREATE TABLE a4 PARTITION OF ph FOR VALUES WITH (MODULUS 8, REMAINDER 3);
             CREATE TABLE a5 PARTITION OF ps FOR VALUES IN (1);
             ALTER TABLE pl ATTACH PARTITION free FOR VALUES IN (6);
             ALTER TABLE pl DETACH PARTITION free;
             ALTER TABLE pl ATTACH PARTITION free FOR VALUES IN (6, 7);
             DROP TABLE l3;
             CREATE TABLE a6 PARTITION OF pl FOR VALUES IN (2);",
        ),
    ]);
}

#[test]
fn rules_are_validated_and_tracked() {
    // PG 18 transformRuleStmt / DefineQueryRewrite / InsertRule /
    // RenameRewriteRule / get_rewrite_oid / EnableDisableRule.
    let setup = "CREATE TABLE t (a int);
                 CREATE VIEW v AS SELECT 1 AS a;
                 CREATE TABLE pt (a int) PARTITION BY LIST (a);
                 CREATE SEQUENCE s;
                 CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a;
                 CREATE RULE r AS ON INSERT TO t DO INSTEAD NOTHING;
                 CREATE RULE q AS ON DELETE TO t DO INSTEAD NOTHING;";
    for (stmt, msg) in [
        (
            "CREATE RULE x AS ON INSERT TO nosuch DO INSTEAD NOTHING;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE RULE r AS ON INSERT TO t DO INSTEAD NOTHING;",
            "rule \"r\" for relation \"t\" already exists",
        ),
        (
            "CREATE RULE x AS ON INSERT TO s DO INSTEAD NOTHING;",
            "relation \"s\" cannot have rules",
        ),
        (
            "CREATE RULE x AS ON INSERT TO mv DO INSTEAD NOTHING;",
            "rules on materialized views are not supported",
        ),
        (
            "CREATE RULE x AS ON INSERT TO t WHERE nosuch > 0 DO INSTEAD NOTHING;",
            "column \"nosuch\" does not exist",
        ),
        (
            "CREATE RULE x AS ON INSERT TO t WHERE new.a DO INSTEAD NOTHING;",
            "argument of WHERE must be type boolean, not type integer",
        ),
        (
            "CREATE RULE x AS ON INSERT TO t WHERE old.a > 0 DO INSTEAD NOTHING;",
            "invalid reference to FROM-clause entry for table \"old\"",
        ),
        (
            "CREATE RULE x AS ON DELETE TO t DO INSTEAD SELECT new.a;",
            "ON DELETE rule cannot use NEW",
        ),
        (
            "CREATE RULE x AS ON INSERT TO t DO INSTEAD SELECT old.a;",
            "ON INSERT rule cannot use OLD",
        ),
        (
            "CREATE RULE x AS ON DELETE TO t WHERE new.a > 0 DO INSTEAD NOTHING;",
            "invalid reference to FROM-clause entry for table \"new\"",
        ),
        (
            "CREATE RULE x AS ON INSERT TO t DO INSTEAD INSERT INTO nosuch VALUES (1);",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE RULE \"_RETURN\" AS ON SELECT TO t DO INSTEAD SELECT 1 AS a;",
            "relation \"t\" cannot have ON SELECT rules",
        ),
        (
            "ALTER RULE nosuch ON t RENAME TO z;",
            "rule \"nosuch\" for relation \"t\" does not exist",
        ),
        (
            "ALTER RULE r ON t RENAME TO q;",
            "rule \"q\" for relation \"t\" already exists",
        ),
        (
            "ALTER TABLE t DISABLE RULE nosuch;",
            "rule \"nosuch\" for relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t ENABLE RULE nosuch;",
            "rule \"nosuch\" for relation \"t\" does not exist",
        ),
        (
            "DROP RULE nosuch ON t;",
            "rule \"nosuch\" for relation \"t\" does not exist",
        ),
        (
            "ALTER RULE r ON t RENAME TO rr; DROP RULE r ON t;",
            "rule \"r\" for relation \"t\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE OR REPLACE RULE r AS ON UPDATE TO t DO INSTEAD NOTHING;
             CREATE RULE vr AS ON INSERT TO v DO INSTEAD NOTHING;
             CREATE RULE pr AS ON INSERT TO pt DO INSTEAD NOTHING;
             CREATE RULE w AS ON UPDATE TO t WHERE old.a <> new.a DO ALSO
                 WITH c AS (SELECT 1) SELECT * FROM c;
             ALTER RULE r ON t RENAME TO rr;
             ALTER TABLE t DISABLE RULE rr;
             ALTER TABLE t ENABLE ALWAYS RULE rr;
             DROP RULE IF EXISTS nosuch ON t;
             DROP RULE IF EXISTS nosuch ON nosuch;
             DROP RULE rr ON t;",
        ),
    ]);
}

#[test]
fn alter_owner_and_comment_resolve_their_target() {
    // PG 18 get_object_address / LookupFuncWithArgs / AlterTypeOwner /
    // get_collation_oid / get_trigger_oid / get_relation_policy_oid /
    // get_rewrite_oid / LookupOperName.
    let setup = "CREATE TABLE t (a int);
                 CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';
                 CREATE FUNCTION g(int) RETURNS int LANGUAGE sql AS 'select 1';
                 CREATE FUNCTION g(text) RETURNS int LANGUAGE sql AS 'select 1';
                 CREATE PROCEDURE pr(int) LANGUAGE sql AS 'select 1';";
    for (stmt, msg) in [
        (
            "ALTER FUNCTION nosuch(int) OWNER TO postgres;",
            "function nosuch(integer) does not exist",
        ),
        (
            "ALTER FUNCTION nosuch OWNER TO postgres;",
            "could not find a function named \"nosuch\"",
        ),
        (
            "ALTER FUNCTION g OWNER TO postgres;",
            "function name \"g\" is not unique",
        ),
        (
            "ALTER PROCEDURE nosuch() OWNER TO postgres;",
            "procedure nosuch() does not exist",
        ),
        (
            "ALTER ROUTINE nosuch() OWNER TO postgres;",
            "function nosuch() does not exist",
        ),
        (
            "ALTER AGGREGATE nosuch(int) OWNER TO postgres;",
            "aggregate nosuch(integer) does not exist",
        ),
        (
            "ALTER AGGREGATE f(int) OWNER TO postgres;",
            "function f(integer) is not an aggregate",
        ),
        (
            "ALTER PROCEDURE f(int) OWNER TO postgres;",
            "f(integer) is not a procedure",
        ),
        (
            "ALTER FUNCTION pr(int) OWNER TO postgres;",
            "pr(integer) is not a function",
        ),
        (
            "COMMENT ON FUNCTION pr(int) IS 'x';",
            "pr(integer) is not a function",
        ),
        (
            "GRANT EXECUTE ON FUNCTION pr(int) TO public;",
            "pr(integer) is not a function",
        ),
        (
            "GRANT EXECUTE ON PROCEDURE f(int) TO public;",
            "f(integer) is not a procedure",
        ),
        (
            "ALTER SCHEMA nosuch OWNER TO postgres;",
            "schema \"nosuch\" does not exist",
        ),
        (
            "ALTER TYPE nosuch OWNER TO postgres;",
            "type \"nosuch\" does not exist",
        ),
        (
            "ALTER TYPE public.nosuch OWNER TO postgres;",
            "type \"public.nosuch\" does not exist",
        ),
        (
            "ALTER DOMAIN nosuch OWNER TO postgres;",
            "type \"nosuch\" does not exist",
        ),
        (
            "ALTER COLLATION nosuch OWNER TO postgres;",
            "collation \"nosuch\" for encoding \"UTF8\" does not exist",
        ),
        (
            "ALTER OPERATOR +(int, nosuch) OWNER TO postgres;",
            "type \"nosuch\" does not exist",
        ),
        (
            "ALTER OPERATOR ###(int, int) OWNER TO postgres;",
            "operator does not exist: integer ### integer",
        ),
        (
            "ALTER SEQUENCE nosuch OWNER TO postgres;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER VIEW nosuch OWNER TO postgres;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON AGGREGATE nosuch(int) IS 'x';",
            "aggregate nosuch(integer) does not exist",
        ),
        (
            "COMMENT ON COLLATION nosuch IS 'x';",
            "collation \"nosuch\" for encoding \"UTF8\" does not exist",
        ),
        (
            "COMMENT ON TRIGGER nosuch ON t IS 'x';",
            "trigger \"nosuch\" for table \"t\" does not exist",
        ),
        (
            "COMMENT ON POLICY nosuch ON t IS 'x';",
            "policy \"nosuch\" for table \"t\" does not exist",
        ),
        (
            "COMMENT ON RULE nosuch ON t IS 'x';",
            "rule \"nosuch\" for relation \"t\" does not exist",
        ),
        (
            "COMMENT ON OPERATOR ###(int, int) IS 'x';",
            "operator does not exist: integer ### integer",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER FUNCTION f(int) OWNER TO postgres;
             ALTER FUNCTION f OWNER TO postgres;
             ALTER ROUTINE f(int) OWNER TO postgres;
             ALTER AGGREGATE count(*) OWNER TO postgres;
             ALTER SCHEMA public OWNER TO postgres;
             ALTER TYPE int4 OWNER TO postgres;
             ALTER OPERATOR +(int, int) OWNER TO postgres;
             ALTER COLLATION \"C\" OWNER TO postgres;
             COMMENT ON OPERATOR -(NONE, int) IS 'x';
             COMMENT ON COLLATION \"C\" IS 'x';
             COMMENT ON AGGREGATE sum(int) IS 'x';
             ALTER FUNCTION sum(int) OWNER TO postgres;
             ALTER ROUTINE pr(int) OWNER TO postgres;
             GRANT EXECUTE ON ROUTINE pr(int) TO public;",
        ),
    ]);
}

#[test]
fn migration_queries_are_analyzed() {
    // PG 18 runs parse analysis on every DML / SELECT / CALL statement of
    // a migration.
    let setup = "CREATE TABLE t (a int NOT NULL, b text);";
    for (stmt, msg) in [
        (
            "INSERT INTO nosuch VALUES (1);",
            "relation \"nosuch\" does not exist",
        ),
        (
            "INSERT INTO t (nosuch) VALUES (1);",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "INSERT INTO t VALUES ('x');",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "INSERT INTO t VALUES (1, 'a', 3);",
            "INSERT has more expressions than target columns",
        ),
        (
            "UPDATE t SET nosuch = 1;",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "UPDATE t SET a = 'x';",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "DELETE FROM t WHERE nosuch = 1;",
            "column \"nosuch\" does not exist",
        ),
        (
            "DELETE FROM t WHERE a;",
            "argument of WHERE must be type boolean, not type integer",
        ),
        ("SELECT nosuch FROM t;", "column \"nosuch\" does not exist"),
        (
            "SELECT * FROM nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        ("CALL nosuch();", "procedure nosuch() does not exist"),
        (
            "MERGE INTO t USING t AS s ON t.a = s.a WHEN MATCHED THEN UPDATE SET nosuch = 1;",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "INSERT INTO t VALUES (1, 'x') RETURNING nosuch;",
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
            "INSERT INTO t VALUES (1, 'x'), (2, NULL);
             INSERT INTO t SELECT 1, 'x' WHERE false;
             UPDATE t SET b = b || '!' WHERE a > 1;
             DELETE FROM t WHERE b IS NULL;
             SELECT set_config('search_path', 'public', false);
             SELECT count(*) FROM t;",
        ),
    ]);
}

#[test]
fn maintenance_statements_resolve_their_targets() {
    // PG 18 ExecuteTruncate / cluster / ReindexIndex / vacuum /
    // LockTableCommand / ExecSecLabelStmt / SetDefaultACLsInSchemas.
    let setup = "CREATE TABLE r (id int PRIMARY KEY);
                 CREATE TABLE f (x int REFERENCES r);
                 CREATE VIEW v AS SELECT 1 AS a;
                 CREATE SEQUENCE s;
                 CREATE TABLE t (a int);
                 CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a;
                 CREATE INDEX ti ON t (a);
                 CREATE INDEX tp ON t (a) WHERE a > 0;";
    for (stmt, msg) in [
        ("TRUNCATE nosuch;", "relation \"nosuch\" does not exist"),
        ("TRUNCATE v;", "\"v\" is not a table"),
        ("TRUNCATE s;", "\"s\" is not a table"),
        ("TRUNCATE mv;", "\"mv\" is not a table"),
        (
            "TRUNCATE r;",
            "cannot truncate a table referenced in a foreign key constraint",
        ),
        ("CLUSTER nosuch;", "relation \"nosuch\" does not exist"),
        (
            "CLUSTER t;",
            "there is no previously clustered index for table \"t\"",
        ),
        (
            "CLUSTER mv;",
            "there is no previously clustered index for table \"mv\"",
        ),
        (
            "CLUSTER v USING ti;",
            "\"v\" is not a table or materialized view",
        ),
        (
            "CLUSTER t USING nosuch_idx;",
            "index \"nosuch_idx\" for table \"t\" does not exist",
        ),
        (
            "CLUSTER t USING tp;",
            "cannot cluster on partial index \"tp\"",
        ),
        (
            "ALTER TABLE t CLUSTER ON ti; ALTER TABLE t SET WITHOUT CLUSTER; CLUSTER t;",
            "there is no previously clustered index for table \"t\"",
        ),
        (
            "CLUSTER t USING ti; DROP INDEX ti; CLUSTER t;",
            "there is no previously clustered index for table \"t\"",
        ),
        (
            "REINDEX INDEX nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        ("REINDEX INDEX t;", "\"t\" is not an index"),
        (
            "REINDEX TABLE v;",
            "\"v\" is not a table or materialized view",
        ),
        (
            "REINDEX TABLE s;",
            "\"s\" is not a table or materialized view",
        ),
        ("ANALYZE nosuch;", "relation \"nosuch\" does not exist"),
        (
            "ANALYZE t (nosuch);",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "SELECT 1; LOCK TABLE nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        ("SELECT 1; LOCK TABLE s;", "cannot lock relation \"s\""),
        ("SELECT 1; LOCK TABLE mv;", "cannot lock relation \"mv\""),
        (
            "SECURITY LABEL ON TABLE t IS 'x';",
            "no security label providers have been loaded",
        ),
        (
            "ALTER DEFAULT PRIVILEGES IN SCHEMA nosuch GRANT SELECT ON TABLES TO public;",
            "schema \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "TRUNCATE r, f;
             TRUNCATE r CASCADE;
             TRUNCATE ONLY t;
             CLUSTER t USING ti;
             CLUSTER t;
             ALTER TABLE t CLUSTER ON ti;
             REINDEX TABLE t;
             REINDEX INDEX ti;
             REINDEX TABLE mv;
             ANALYZE t;
             ANALYZE t (a);
             ANALYZE v;
             LOCK TABLE t, v IN SHARE MODE;
             ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO public;",
        ),
    ]);
}

#[test]
fn transaction_block_rules_follow_the_migration_runner() {
    // PG 18 PreventInTransactionBlock / RequireTransactionBlock: a
    // migration of several statements runs inside a transaction block
    // (the runner's, or the implicit block of one multi-statement query).
    let setup = "CREATE TABLE t (a int);
                 CREATE TABLE pt (a int) PARTITION BY LIST (a);
                 CREATE TABLE p1 PARTITION OF pt FOR VALUES IN (1);
                 CREATE INDEX ti ON t (a);";
    for (stmt, msg) in [
        (
            "SELECT 1; VACUUM t;",
            "VACUUM cannot run inside a transaction block",
        ),
        (
            "SELECT 1; CREATE INDEX CONCURRENTLY x ON t (a);",
            "CREATE INDEX CONCURRENTLY cannot run inside a transaction block",
        ),
        (
            "-- no-transaction\nSELECT 1; CREATE INDEX CONCURRENTLY x ON t (a);",
            "CREATE INDEX CONCURRENTLY cannot run inside a transaction block",
        ),
        (
            "SELECT 1; DROP INDEX CONCURRENTLY ti;",
            "DROP INDEX CONCURRENTLY cannot run inside a transaction block",
        ),
        (
            "SELECT 1; REINDEX INDEX CONCURRENTLY ti;",
            "REINDEX CONCURRENTLY cannot run inside a transaction block",
        ),
        (
            "SELECT 1; REINDEX SCHEMA public;",
            "REINDEX SCHEMA cannot run inside a transaction block",
        ),
        (
            "SELECT 1; CLUSTER;",
            "CLUSTER cannot run inside a transaction block",
        ),
        (
            "SELECT 1; ALTER TABLE pt DETACH PARTITION p1 CONCURRENTLY;",
            "ALTER TABLE ... DETACH CONCURRENTLY cannot run inside a transaction block",
        ),
        (
            "SELECT 1; DISCARD ALL;",
            "DISCARD ALL cannot run inside a transaction block",
        ),
        (
            "-- no-transaction\nLOCK TABLE t;",
            "LOCK TABLE can only be used in transaction blocks",
        ),
        (
            "-- no-transaction\nDECLARE c CURSOR FOR SELECT 1;",
            "DECLARE CURSOR can only be used in transaction blocks",
        ),
        (
            "-- no-transaction\nSAVEPOINT a;",
            "SAVEPOINT can only be used in transaction blocks",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    for ok in [
        "-- no-transaction\nCREATE INDEX CONCURRENTLY x ON t (a);",
        "-- no-transaction\nVACUUM t;",
        "-- no-transaction\nDROP INDEX CONCURRENTLY ti;",
        "SELECT 1; LOCK TABLE t;",
        "SELECT 1; ANALYZE t;",
    ] {
        build_db(&[("0001.sql", setup), ("0002.sql", ok)]);
    }
}

#[test]
fn rename_constraint_needs_a_free_name() {
    // PG 18 RenameConstraintById / RenameRelationInternal /
    // get_domain_constraint_oid.
    let setup = "CREATE TABLE t (a int, b int, CONSTRAINT c1 CHECK (a > 0),
                                 CONSTRAINT c2 CHECK (b > 0), CONSTRAINT u UNIQUE (a));
                 CREATE INDEX ti ON t (b);
                 CREATE DOMAIN d AS int CONSTRAINT d1 CHECK (VALUE > 0) CONSTRAINT d2 CHECK (VALUE < 9);";
    for (stmt, msg) in [
        (
            "ALTER TABLE t RENAME CONSTRAINT c1 TO c2;",
            "constraint \"c2\" for relation \"t\" already exists",
        ),
        (
            "ALTER TABLE t RENAME CONSTRAINT u TO ti;",
            "relation \"ti\" already exists",
        ),
        (
            "ALTER TABLE t RENAME CONSTRAINT c1 TO u;",
            "constraint \"u\" for relation \"t\" already exists",
        ),
        (
            "ALTER TABLE t RENAME CONSTRAINT u TO c2;",
            "constraint \"c2\" for relation \"t\" already exists",
        ),
        (
            "ALTER DOMAIN d RENAME CONSTRAINT nosuch TO z;",
            "constraint \"nosuch\" for domain d does not exist",
        ),
        (
            "ALTER DOMAIN d RENAME CONSTRAINT d1 TO d2;",
            "constraint \"d2\" for domain d already exists",
        ),
        (
            "ALTER DOMAIN d RENAME CONSTRAINT d1 TO d3; ALTER DOMAIN d DROP CONSTRAINT d1;",
            "constraint \"d1\" of domain \"d\" does not exist",
        ),
        (
            "ALTER DOMAIN nosuch RENAME CONSTRAINT d1 TO d3;",
            "type \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE t RENAME CONSTRAINT c1 TO c3;
             ALTER TABLE t RENAME CONSTRAINT u TO u2;
             ALTER DOMAIN d RENAME CONSTRAINT d1 TO d3;
             ALTER DOMAIN d DROP CONSTRAINT d3;",
        ),
    ]);
}

#[test]
fn access_methods_and_operator_classes() {
    // PG 18 CreateAccessMethod / CreateOpFamily / DefineOpClass /
    // DefineIndex / ResolveOpClass / GetDefaultOpClass.
    let setup = "CREATE TABLE t (a int, j json, v int[], e text, r int4range);
                 CREATE TYPE mood AS ENUM ('a', 'b');
                 CREATE TABLE m (x mood);
                 CREATE OPERATOR FAMILY f1 USING btree;";
    for (stmt, msg) in [
        (
            "CREATE ACCESS METHOD x TYPE INDEX HANDLER nosuch;",
            "function nosuch(internal) does not exist",
        ),
        (
            "CREATE ACCESS METHOD y TYPE INDEX HANDLER heap_tableam_handler;",
            "function heap_tableam_handler must return type index_am_handler",
        ),
        (
            "CREATE ACCESS METHOD heap2 TYPE TABLE HANDLER heap_tableam_handler;
             CREATE ACCESS METHOD heap2 TYPE TABLE HANDLER heap_tableam_handler;",
            "access method \"heap2\" already exists",
        ),
        (
            "CREATE OPERATOR CLASS c1 FOR TYPE int4 USING nosuch AS OPERATOR 1 <;",
            "access method \"nosuch\" does not exist",
        ),
        (
            "CREATE OPERATOR CLASS c1 FOR TYPE nosuch USING btree AS OPERATOR 1 <;",
            "type \"nosuch\" does not exist",
        ),
        (
            "CREATE OPERATOR CLASS c2 DEFAULT FOR TYPE int4 USING btree AS OPERATOR 1 <;",
            "could not make operator class \"c2\" be default for type int4",
        ),
        (
            "CREATE OPERATOR CLASS c3 FOR TYPE int4 USING btree FAMILY nosuch AS OPERATOR 1 <;",
            "operator family \"nosuch\" does not exist for access method \"btree\"",
        ),
        (
            "CREATE OPERATOR FAMILY f1 USING btree;",
            "operator family \"f1\" for access method \"btree\" already exists",
        ),
        (
            "CREATE OPERATOR FAMILY f2 USING nosuch;",
            "access method \"nosuch\" does not exist",
        ),
        (
            "CREATE OPERATOR CLASS c4 FOR TYPE int4 USING btree AS OPERATOR 1 <;
             CREATE OPERATOR CLASS c4 FOR TYPE int4 USING btree AS OPERATOR 1 <;",
            "operator class \"c4\" for access method \"btree\" already exists",
        ),
        (
            "DROP OPERATOR CLASS nosuch USING btree;",
            "operator class \"nosuch\" does not exist for access method \"btree\"",
        ),
        (
            "DROP OPERATOR FAMILY nosuch USING btree;",
            "operator family \"nosuch\" does not exist for access method \"btree\"",
        ),
        (
            "DROP ACCESS METHOD nosuch;",
            "access method \"nosuch\" does not exist",
        ),
        (
            "DROP OPERATOR CLASS c4 USING nosuch;",
            "access method \"nosuch\" does not exist",
        ),
        (
            "CREATE INDEX ON t USING nosuch (a);",
            "access method \"nosuch\" does not exist",
        ),
        (
            "CREATE INDEX ON t (a nosuch_ops);",
            "operator class \"nosuch_ops\" does not exist for access method \"btree\"",
        ),
        (
            "CREATE INDEX ON t (a text_ops);",
            "operator class \"text_ops\" does not accept data type integer",
        ),
        (
            "CREATE INDEX ON t (j);",
            "data type json has no default operator class for access method \"btree\"",
        ),
        (
            "CREATE INDEX ON t USING gist (a);",
            "data type integer has no default operator class for access method \"gist\"",
        ),
        (
            "CREATE INDEX ON t USING hash (a DESC);",
            "access method \"hash\" does not support ASC/DESC options",
        ),
        (
            "CREATE INDEX ON t USING hash (a NULLS FIRST);",
            "access method \"hash\" does not support NULLS FIRST/LAST options",
        ),
        (
            "CREATE INDEX ON t USING gin (v) INCLUDE (a);",
            "access method \"gin\" does not support included columns",
        ),
        (
            "CREATE INDEX ON t USING spgist (e, a);",
            "access method \"spgist\" does not support multicolumn indexes",
        ),
        (
            "CREATE UNIQUE INDEX ON t USING hash (a);",
            "access method \"hash\" does not support unique indexes",
        ),
        (
            "CREATE INDEX ON t (a) INCLUDE (nosuch);",
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
            "CREATE OPERATOR CLASS c4 FOR TYPE int4 USING btree FAMILY f1 AS OPERATOR 1 <;
             DROP OPERATOR CLASS c4 USING btree;
             DROP OPERATOR FAMILY f1 USING btree;
             DROP OPERATOR CLASS IF EXISTS nosuch USING btree;
             CREATE INDEX ON t (a);
             CREATE INDEX ON t (e varchar_ops);
             CREATE INDEX ON t USING gin (v);
             CREATE INDEX ON t (v);
             CREATE INDEX ON t (r);
             CREATE INDEX ON t USING gist (r);
             CREATE INDEX ON t USING brin (a);
             CREATE INDEX ON t USING spgist (e);
             CREATE INDEX ON t USING hash (e);
             CREATE INDEX ON t (a DESC NULLS LAST) INCLUDE (e);
             CREATE INDEX ON t ((a + 1));
             CREATE INDEX ON t ((j->>'k'));
             CREATE INDEX ON m (x);
             CREATE EXTENSION pg_trgm;
             CREATE INDEX ON t USING gin (e gin_trgm_ops);
             CREATE EXTENSION btree_gist;
             CREATE INDEX ON t USING gist (a);",
        ),
    ]);
}

#[test]
fn extended_statistics_are_validated_and_tracked() {
    // PG 18 CreateStatistics / AlterStatistics / get_statistics_object_oid.
    let setup = "CREATE TABLE t (a int, b int, c int, j json);
                 CREATE VIEW v AS SELECT 1 AS a, 2 AS b;
                 CREATE TABLE u (x int, y int);
                 CREATE SCHEMA s2;
                 CREATE STATISTICS st ON a, b FROM t;
                 CREATE STATISTICS ON a, b FROM t;";
    for (stmt, msg) in [
        (
            "CREATE STATISTICS s1 ON a, b FROM v;",
            "cannot define statistics for relation \"v\"",
        ),
        (
            "CREATE STATISTICS s2 ON a, b FROM t, u;",
            "only a single relation is allowed in CREATE STATISTICS",
        ),
        (
            "CREATE STATISTICS s0 ON a, b FROM nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE STATISTICS s3 ON a, a FROM t;",
            "duplicate column name in statistics definition",
        ),
        (
            "CREATE STATISTICS s3 ON (a + 1), (a + 1) FROM t;",
            "duplicate expression in statistics definition",
        ),
        (
            "CREATE STATISTICS s4 ON a, ctid FROM t;",
            "statistics creation on system columns is not supported",
        ),
        (
            "CREATE STATISTICS s5 ON a, j FROM t;",
            "column \"j\" cannot be used in statistics because its type json has no default btree \
             operator class",
        ),
        (
            "CREATE STATISTICS s6 (nosuch) ON a, b FROM t;",
            "unrecognized statistics kind \"nosuch\"",
        ),
        (
            "CREATE STATISTICS s8 (ndistinct) ON (a + 1) FROM t;",
            "when building statistics on a single expression, statistics kinds may not be specified",
        ),
        (
            "CREATE STATISTICS s9 ON a FROM t;",
            "extended statistics require at least 2 columns",
        ),
        (
            "CREATE STATISTICS s9 ON a, nosuch FROM t;",
            "column \"nosuch\" does not exist",
        ),
        (
            "CREATE STATISTICS st ON a, c FROM t;",
            "statistics object \"st\" already exists",
        ),
        (
            "CREATE STATISTICS t_a_b_stat ON a, c FROM t;",
            "statistics object \"t_a_b_stat\" already exists",
        ),
        (
            "DROP STATISTICS nosuch;",
            "statistics object \"nosuch\" does not exist",
        ),
        (
            "ALTER STATISTICS nosuch SET STATISTICS 5;",
            "statistics object \"nosuch\" does not exist",
        ),
        (
            "ALTER STATISTICS st RENAME TO t_a_b_stat;",
            "statistics object \"t_a_b_stat\" already exists in schema \"public\"",
        ),
        (
            "COMMENT ON STATISTICS nosuch IS 'x';",
            "statistics object \"nosuch\" does not exist",
        ),
        (
            "ALTER STATISTICS nosuch OWNER TO postgres;",
            "statistics object \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE t DROP COLUMN b; DROP STATISTICS st;",
            "statistics object \"st\" does not exist",
        ),
        (
            "DROP TABLE t; DROP STATISTICS st;",
            "statistics object \"st\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE STATISTICS ON a, b FROM t;
             DROP STATISTICS t_a_b_stat1;
             CREATE STATISTICS s2.st ON a, b FROM t;
             CREATE STATISTICS IF NOT EXISTS st ON a, b FROM t;
             CREATE STATISTICS s7 ON (a + 1) FROM t;
             CREATE STATISTICS ON (a + 1), (b + 1) FROM t;
             CREATE STATISTICS (ndistinct, mcv) ON a, (b + 1) FROM t;
             ALTER STATISTICS st SET STATISTICS 5;
             ALTER STATISTICS IF EXISTS nosuch SET STATISTICS 5;
             ALTER STATISTICS st RENAME TO st2;
             COMMENT ON STATISTICS st2 IS 'x';
             ALTER TABLE t DROP COLUMN c;
             DROP STATISTICS st2, t_a_b_stat, t_expr_expr_stat, t_a_expr_stat, s7, s2.st;
             DROP STATISTICS IF EXISTS nosuch;",
        ),
    ]);
}

#[test]
fn storage_parameters_are_validated() {
    // PG 18 transformRelOptions / heap_reloptions / index_reloptions /
    // view_reloptions / partitioned_table_reloptions / parse_one_reloption.
    let setup = "CREATE TABLE t (a int);
                 CREATE VIEW v AS SELECT a FROM t;
                 CREATE INDEX ti ON t (a);
                 CREATE INDEX tg ON t USING gin ((ARRAY[a]));";
    for (stmt, msg) in [
        (
            "CREATE TABLE p (a int) PARTITION BY LIST (a) WITH (fillfactor = 50);",
            "cannot specify storage parameters for a partitioned table",
        ),
        (
            "CREATE TABLE x (a int) WITH (autovacuum_enabled = maybe);",
            "invalid value for boolean option \"autovacuum_enabled\": maybe",
        ),
        (
            "CREATE TABLE x (a int) WITH (fillfactor);",
            "invalid value for integer option \"fillfactor\": true",
        ),
        (
            "CREATE TABLE x (a int) WITH (oids = true);",
            "tables declared WITH OIDS are not supported",
        ),
        (
            "CREATE TABLE x (a int) WITH (toast.fillfactor = 50);",
            "unrecognized parameter \"fillfactor\"",
        ),
        (
            "CREATE TABLE x (a int) WITH (foo.fillfactor = 50);",
            "unrecognized parameter namespace \"foo\"",
        ),
        (
            "CREATE TABLE x (a int) WITH (autovacuum_vacuum_cost_delay = 200);",
            "value 200 out of bounds for option \"autovacuum_vacuum_cost_delay\"",
        ),
        (
            "CREATE TABLE x (a int) WITH (vacuum_index_cleanup = sometimes);",
            "invalid value for enum option \"vacuum_index_cleanup\": sometimes",
        ),
        (
            "CREATE TABLE x (a int) WITH (nosuchopt = 1);",
            "unrecognized parameter \"nosuchopt\"",
        ),
        (
            "CREATE TABLE x (a int) WITH (fillfactor = 50, fillfactor = 60);",
            "parameter \"fillfactor\" specified more than once",
        ),
        (
            "ALTER TABLE t SET (fillfactor = 5);",
            "value 5 out of bounds for option \"fillfactor\"",
        ),
        (
            "ALTER TABLE t SET (nosuchopt = 5);",
            "unrecognized parameter \"nosuchopt\"",
        ),
        (
            "ALTER TABLE t RESET (fillfactor = 5);",
            "RESET must not include values for parameters",
        ),
        (
            "CREATE INDEX ON t USING gist ((point(a, a))) WITH (buffering = maybe);",
            "invalid value for enum option \"buffering\": maybe",
        ),
        (
            "CREATE INDEX ON t (a) WITH (fillfactor = 5);",
            "value 5 out of bounds for option \"fillfactor\"",
        ),
        (
            "CREATE INDEX ON t (a) WITH (nosuchopt = 1);",
            "unrecognized parameter \"nosuchopt\"",
        ),
        (
            "CREATE INDEX ON t USING hash (a) WITH (deduplicate_items = off);",
            "unrecognized parameter \"deduplicate_items\"",
        ),
        (
            "ALTER INDEX ti SET (fastupdate = off);",
            "unrecognized parameter \"fastupdate\"",
        ),
        (
            "ALTER INDEX tg SET (fillfactor = 50);",
            "unrecognized parameter \"fillfactor\"",
        ),
        (
            "CREATE VIEW x WITH (fillfactor = 10) AS SELECT 1 AS a;",
            "unrecognized parameter \"fillfactor\"",
        ),
        (
            "ALTER VIEW v SET (security_barrier = maybe);",
            "invalid value for boolean option \"security_barrier\": maybe",
        ),
        (
            "CREATE MATERIALIZED VIEW x WITH (fillfactor = 5) AS SELECT 1 AS a;",
            "value 5 out of bounds for option \"fillfactor\"",
        ),
        (
            "CREATE TABLE x WITH (autovacuum_vacuum_scale_factor = 200) AS SELECT 1 AS a;",
            "value 200 out of bounds for option \"autovacuum_vacuum_scale_factor\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE a1 (a int) WITH (oids = false);
             CREATE TABLE a2 (a int) WITH (toast.autovacuum_enabled = off, autovacuum_vacuum_cost_delay = 20);
             CREATE TABLE a3 (a int) WITH (fillfactor = 50.5, autovacuum_enabled = yes);
             CREATE TABLE a4 (a int) WITH (fillfactor = '70', vacuum_index_cleanup = off);
             ALTER TABLE t SET (fillfactor = 70, parallel_workers = 4);
             ALTER TABLE t RESET (fillfactor, nosuch);
             CREATE INDEX ON t (a) WITH (deduplicate_items = off, fillfactor = 90);
             CREATE INDEX ON t USING brin (a) WITH (pages_per_range = 32, autosummarize = on);
             ALTER INDEX ti SET (fillfactor = 80);
             ALTER INDEX tg SET (fastupdate = off);
             ALTER VIEW v SET (security_barrier = true, security_invoker = on);
             CREATE MATERIALIZED VIEW m1 WITH (fillfactor = 50) AS SELECT 1 AS a;",
        ),
    ]);
}

#[test]
fn partitioned_indexes_reach_the_partitions() {
    // PG 18 DefineIndex recursion / AttachPartitionEnsureIndexes: every
    // partition gets (or attaches) a copy of each partitioned index, with
    // an inherited constraint for constraint indexes.
    let setup = "CREATE TABLE p (a int PRIMARY KEY, b int) PARTITION BY LIST (a);
                 CREATE TABLE p1 PARTITION OF p FOR VALUES IN (1);
                 CREATE UNIQUE INDEX pb ON p (a, b);
                 CREATE TABLE p2 (a int NOT NULL, b int);
                 ALTER TABLE p ATTACH PARTITION p2 FOR VALUES IN (2);
                 CREATE TABLE p3 (a int NOT NULL, b int);
                 CREATE UNIQUE INDEX p3_own ON p3 (a, b);
                 ALTER TABLE p ATTACH PARTITION p3 FOR VALUES IN (3);";
    for (stmt, msg) in [
        (
            "ALTER TABLE p1 DROP CONSTRAINT p1_pkey;",
            "cannot drop inherited constraint \"p1_pkey\" of relation \"p1\"",
        ),
        (
            "DROP INDEX p1_pkey;",
            "cannot drop index p1_pkey because index p_pkey requires it",
        ),
        (
            "DROP INDEX p1_a_b_idx;",
            "cannot drop index p1_a_b_idx because index pb requires it",
        ),
        (
            "DROP INDEX p3_own;",
            "cannot drop index p3_own because index pb requires it",
        ),
        (
            "DROP INDEX pb; INSERT INTO p1 VALUES (1) ON CONFLICT (a, b) DO NOTHING;",
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        ),
        (
            "ALTER TABLE p DROP CONSTRAINT p_pkey; INSERT INTO p1 VALUES (1) ON CONFLICT (a) DO NOTHING;",
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "INSERT INTO p1 VALUES (1, 1) ON CONFLICT (a, b) DO NOTHING;
             INSERT INTO p1 VALUES (1, 1) ON CONFLICT (a) DO NOTHING;
             INSERT INTO p2 VALUES (2) ON CONFLICT (a) DO NOTHING;
             INSERT INTO p3 VALUES (3) ON CONFLICT (a, b) DO NOTHING;
             CREATE INDEX pbb ON p (b);
             DROP INDEX pbb;
             ALTER TABLE p DETACH PARTITION p2;
             ALTER TABLE p2 DROP CONSTRAINT p2_pkey;
             DROP INDEX p2_a_b_idx;
             CREATE TABLE p4 PARTITION OF p FOR VALUES IN (4);
             INSERT INTO p4 VALUES (4) ON CONFLICT (a) DO NOTHING;",
        ),
    ]);
}

#[test]
fn table_access_methods_are_resolved() {
    // PG 18 get_table_am_oid: USING / SET ACCESS METHOD name a table AM.
    let setup = "CREATE TABLE t (a int); CREATE VIEW v AS SELECT 1 AS a;";
    for (stmt, msg) in [
        (
            "CREATE TABLE x (a int) USING nosuch;",
            "access method \"nosuch\" does not exist",
        ),
        (
            "CREATE TABLE x (a int) USING btree;",
            "access method \"btree\" is not of type TABLE",
        ),
        (
            "ALTER TABLE t SET ACCESS METHOD nosuch;",
            "access method \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE t SET ACCESS METHOD btree;",
            "access method \"btree\" is not of type TABLE",
        ),
        (
            "CREATE MATERIALIZED VIEW x USING nosuch AS SELECT 1;",
            "access method \"nosuch\" does not exist",
        ),
        (
            "CREATE TABLE x USING nosuch AS SELECT 1;",
            "access method \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE v SET ACCESS METHOD heap;",
            "ALTER action SET ACCESS METHOD cannot be performed on relation \"v\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE a1 (a int) USING heap;
             ALTER TABLE t SET ACCESS METHOD heap;
             ALTER TABLE t SET ACCESS METHOD DEFAULT;
             CREATE TABLE pt (a int) PARTITION BY LIST (a) USING heap;
             CREATE MATERIALIZED VIEW m USING heap AS SELECT 1;",
        ),
    ]);
}
