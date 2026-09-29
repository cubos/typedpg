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
fn grant_privileges_must_suit_their_objects() {
    // PG 18 ExecuteGrantStmt (the privileges each object type has),
    // objectNamesToOids, ExecGrant_Relation / _Type_check / _Language_check,
    // merge_acl_with_grant and ExecAlterDefaultPrivilegesStmt.
    let setup = "CREATE TABLE t (a int);
                 CREATE SEQUENCE s;
                 CREATE TYPE mood AS ENUM ('a');
                 CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS 'select 1';";
    for (stmt, msg) in [
        (
            "GRANT EXECUTE ON t TO PUBLIC;",
            "invalid privilege type EXECUTE for relation",
        ),
        (
            "GRANT TRUNCATE (a) ON t TO PUBLIC;",
            "invalid privilege type TRUNCATE for column",
        ),
        (
            "GRANT USAGE ON t TO PUBLIC;",
            "invalid privilege type USAGE for table",
        ),
        (
            "GRANT CONNECT ON SCHEMA public TO PUBLIC;",
            "invalid privilege type CONNECT for schema",
        ),
        (
            "GRANT USAGE ON FUNCTION f(int) TO PUBLIC;",
            "invalid privilege type USAGE for function",
        ),
        (
            "GRANT SELECT ON TYPE mood TO PUBLIC;",
            "invalid privilege type SELECT for type",
        ),
        (
            "GRANT SELECT (a) ON SEQUENCE s TO PUBLIC;",
            "column privileges are only valid for relations",
        ),
        (
            "GRANT USAGE ON TYPE _mood TO PUBLIC;",
            "cannot set privileges of array types",
        ),
        (
            "GRANT USAGE ON DOMAIN mood TO PUBLIC;",
            "\"mood\" is not a domain",
        ),
        (
            "GRANT SELECT ON t TO PUBLIC WITH GRANT OPTION;",
            "grant options can only be granted to roles",
        ),
        (
            "GRANT SELECT ON s TO pg_nosuch;",
            "role \"pg_nosuch\" does not exist",
        ),
        (
            "ALTER DEFAULT PRIVILEGES GRANT USAGE ON TABLES TO PUBLIC;",
            "invalid privilege type USAGE for relation",
        ),
        (
            "ALTER DEFAULT PRIVILEGES GRANT SELECT (a) ON TABLES TO PUBLIC;",
            "default privileges cannot be set for columns",
        ),
        (
            "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT CREATE ON SCHEMAS TO PUBLIC;",
            "cannot use IN SCHEMA clause when using GRANT/REVOKE ON SCHEMAS",
        ),
        (
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC WITH GRANT OPTION;",
            "grant options can only be granted to roles",
        ),
        (
            "GRANT USAGE ON LANGUAGE nosuch TO PUBLIC;",
            "language \"nosuch\" does not exist",
        ),
        (
            "GRANT USAGE ON LANGUAGE c TO PUBLIC;",
            "language \"c\" is not trusted",
        ),
        (
            "GRANT USAGE ON FOREIGN DATA WRAPPER nosuch TO PUBLIC;",
            "foreign-data wrapper \"nosuch\" does not exist",
        ),
        (
            "GRANT USAGE ON FOREIGN SERVER nosuch TO PUBLIC;",
            "server \"nosuch\" does not exist",
        ),
        (
            "GRANT CREATE ON TABLESPACE nosuch TO PUBLIC;",
            "tablespace \"nosuch\" does not exist",
        ),
        (
            "GRANT SET ON PARAMETER nosuch_param TO PUBLIC;",
            "unrecognized configuration parameter \"nosuch_param\"",
        ),
        (
            "GRANT SELECT ON LARGE OBJECT 12345 TO PUBLIC;",
            "large object 12345 does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "GRANT SELECT (ctid, a), UPDATE (a) ON t TO PUBLIC;
             GRANT ALL ON t, s TO PUBLIC;
             GRANT USAGE, SELECT ON SEQUENCE s TO PUBLIC;
             GRANT USAGE ON s TO PUBLIC;
             GRANT USAGE ON TYPE mood TO PUBLIC;
             GRANT USAGE ON LANGUAGE plpgsql TO PUBLIC;
             GRANT CREATE ON TABLESPACE pg_default TO PUBLIC;
             GRANT SET ON PARAMETER work_mem TO PUBLIC;
             GRANT SET ON PARAMETER myapp.flag TO PUBLIC;
             REVOKE SET ON PARAMETER nosuch_param FROM PUBLIC;
             GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO PUBLIC;
             SELECT lo_create(12345);
             GRANT SELECT ON LARGE OBJECT 12345 TO PUBLIC;
             ALTER DEFAULT PRIVILEGES GRANT CREATE ON SCHEMAS TO PUBLIC;
             ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO PUBLIC;",
        ),
    ]);
}

#[test]
fn comment_on_checks_existence_and_relation_kinds() {
    // PG 18 CommentObject and get_object_address for the object kinds
    // besides relations, types and routines.
    let setup = "CREATE TABLE t (a int);
                 CREATE SEQUENCE s;
                 CREATE TYPE mood AS ENUM ('a');
                 CREATE PUBLICATION pb;
                 CREATE FUNCTION ef() RETURNS event_trigger LANGUAGE plpgsql AS 'begin end';
                 CREATE EVENT TRIGGER et ON ddl_command_start EXECUTE FUNCTION ef();";
    for (stmt, msg) in [
        (
            "COMMENT ON COLUMN s.last_value IS 'x';",
            "cannot set comment on relation \"s\"",
        ),
        (
            "COMMENT ON COLUMN t IS 'x';",
            "column name must be qualified",
        ),
        (
            "COMMENT ON CAST (mood AS text) IS 'x';",
            "cast from type mood to type text does not exist",
        ),
        (
            "COMMENT ON CAST (nosuch AS text) IS 'x';",
            "type \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON OPERATOR CLASS int4_ops USING gist IS 'x';",
            "operator class \"int4_ops\" does not exist for access method \"gist\"",
        ),
        (
            "COMMENT ON OPERATOR FAMILY nosuch USING btree IS 'x';",
            "operator family \"nosuch\" does not exist for access method \"btree\"",
        ),
        (
            "COMMENT ON OPERATOR CLASS int4_ops USING nosuch IS 'x';",
            "access method \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON ACCESS METHOD nosuch IS 'x';",
            "access method \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON TABLESPACE nosuch IS 'x';",
            "tablespace \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON FOREIGN DATA WRAPPER nosuch IS 'x';",
            "foreign-data wrapper \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON LARGE OBJECT 1234 IS 'x';",
            "large object 1234 does not exist",
        ),
        (
            "COMMENT ON TRANSFORM FOR int LANGUAGE sql IS 'x';",
            "transform for type integer language \"sql\" does not exist",
        ),
        (
            "COMMENT ON CONVERSION nosuch IS 'x';",
            "conversion \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON EVENT TRIGGER nosuch IS 'x';",
            "event trigger \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON SUBSCRIPTION nosuch IS 'x';",
            "subscription \"nosuch\" does not exist",
        ),
        (
            "COMMENT ON PUBLICATION nope IS 'x';",
            "publication \"nope\" does not exist",
        ),
        (
            "COMMENT ON LANGUAGE nosuch IS 'x';",
            "language \"nosuch\" does not exist",
        ),
        (
            "DROP TRANSFORM FOR int LANGUAGE sql;",
            "transform for type integer language \"sql\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "COMMENT ON COLUMN t.ctid IS 'x';
             COMMENT ON COLUMN t.a IS 'x';
             COMMENT ON CAST (int AS bigint) IS 'x';
             COMMENT ON OPERATOR CLASS int4_ops USING btree IS 'x';
             COMMENT ON OPERATOR FAMILY integer_ops USING btree IS 'x';
             COMMENT ON ACCESS METHOD btree IS 'x';
             COMMENT ON TABLESPACE pg_default IS 'x';
             COMMENT ON CONVERSION utf8_to_iso_8859_1 IS 'x';
             COMMENT ON EVENT TRIGGER et IS 'x';
             COMMENT ON PUBLICATION pb IS 'x';
             COMMENT ON LANGUAGE plpgsql IS 'x';
             DROP TRANSFORM IF EXISTS FOR int LANGUAGE sql;",
        ),
    ]);
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
fn policies_check_their_command_and_depend_on_their_columns() {
    // PG 18 CreatePolicy / AlterPolicy (the expressions each command
    // takes), the policy's dependencies on the columns it reads
    // (ATExecDropColumn, ATExecAlterColumnType), and ATSimplePermissions
    // for the row-security actions.
    let setup = "CREATE TABLE t (a int, b int, c int);
                 CREATE TABLE o (x int);
                 CREATE POLICY pi ON t FOR INSERT WITH CHECK (a > 0);
                 CREATE POLICY ps ON t FOR SELECT USING (b > 0);
                 CREATE POLICY pu ON t FOR UPDATE USING (true)
                     WITH CHECK (EXISTS (SELECT 1 FROM o WHERE o.x = t.c));
                 CREATE FUNCTION f(int) RETURNS bool LANGUAGE sql AS 'select true';
                 CREATE POLICY pf ON t USING (f(c));
                 CREATE VIEW v AS SELECT 1 AS one;";
    for (stmt, msg) in [
        (
            "CREATE POLICY p ON t FOR INSERT USING (a > 0);",
            "only WITH CHECK expression allowed for INSERT",
        ),
        (
            "CREATE POLICY p ON t FOR SELECT WITH CHECK (a > 0);",
            "WITH CHECK cannot be applied to SELECT or DELETE",
        ),
        (
            "CREATE POLICY p ON t FOR DELETE USING (true) WITH CHECK (a > 0);",
            "WITH CHECK cannot be applied to SELECT or DELETE",
        ),
        (
            "CREATE POLICY p ON nosuch FOR INSERT USING (true);",
            "only WITH CHECK expression allowed for INSERT",
        ),
        (
            "ALTER POLICY pi ON t USING (true);",
            "only WITH CHECK expression allowed for INSERT",
        ),
        (
            "ALTER POLICY ps ON t WITH CHECK (true);",
            "only USING expression allowed for SELECT, DELETE",
        ),
        (
            "ALTER POLICY nosuch ON t USING (nosuchcol);",
            "column \"nosuchcol\" does not exist",
        ),
        (
            "ALTER TABLE t DROP COLUMN a;",
            "cannot drop column a of table t because other objects depend on it",
        ),
        (
            "ALTER TABLE o DROP COLUMN x;",
            "cannot drop column x of table o because other objects depend on it",
        ),
        (
            "ALTER TABLE t ALTER COLUMN b TYPE bigint;",
            "cannot alter type of a column used in a policy definition",
        ),
        (
            "ALTER POLICY pu ON t USING (a > 0); ALTER TABLE t DROP COLUMN c;",
            "cannot drop column c of table t because other objects depend on it",
        ),
        (
            "DROP TABLE o;",
            "cannot drop table o because other objects depend on it",
        ),
        (
            "DROP FUNCTION f(int);",
            "cannot drop function f(integer) because other objects depend on it",
        ),
        (
            "DROP TABLE o CASCADE; DROP FUNCTION f(int) CASCADE; ALTER POLICY pu ON t USING (true);",
            "policy \"pu\" for table \"t\" does not exist",
        ),
        (
            "ALTER TABLE v ENABLE ROW LEVEL SECURITY;",
            "ALTER action ENABLE ROW SECURITY cannot be performed on relation \"v\"",
        ),
        (
            "ALTER TABLE v FORCE ROW LEVEL SECURITY;",
            "ALTER action FORCE ROW SECURITY cannot be performed on relation \"v\"",
        ),
        (
            "CREATE POLICY p ON pg_catalog.pg_class USING (true);",
            "permission denied: \"pg_class\" is a system catalog",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE t ENABLE ROW LEVEL SECURITY;
             ALTER TABLE t FORCE ROW LEVEL SECURITY;
             ALTER POLICY pi ON t WITH CHECK (c > 0);
             ALTER TABLE t ALTER COLUMN a TYPE bigint;
             ALTER POLICY pu ON t USING (b > 0) WITH CHECK (true);
             ALTER TABLE o DROP COLUMN x;
             ALTER TABLE t DROP COLUMN b CASCADE;",
        ),
    ]);
    db.analyze("SELECT a, c FROM t").unwrap();
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
fn rule_returning_lists_and_the_return_rule_are_validated() {
    // PG 18 DefineQueryRewrite / checkRuleResultList / RenameRewriteRule,
    // and a view's `_RETURN` rule.
    let setup = "CREATE TABLE t (a int);
                 CREATE TABLE u (a int);
                 CREATE VIEW v AS SELECT a FROM t;
                 CREATE RULE r AS ON INSERT TO t DO INSTEAD NOTHING;";
    for (stmt, msg) in [
        (
            "CREATE RULE x AS ON INSERT TO t DO ALSO INSERT INTO u VALUES (new.a) RETURNING a;",
            "RETURNING lists are not supported in non-INSTEAD rules",
        ),
        (
            "CREATE RULE x AS ON INSERT TO v WHERE new.a > 0
             DO INSTEAD INSERT INTO t VALUES (new.a) RETURNING a;",
            "RETURNING lists are not supported in conditional rules",
        ),
        (
            "CREATE RULE x AS ON INSERT TO v DO INSTEAD (
                 INSERT INTO t VALUES (new.a) RETURNING a;
                 INSERT INTO u VALUES (new.a) RETURNING a);",
            "cannot have multiple RETURNING lists in a rule",
        ),
        (
            "CREATE RULE x AS ON INSERT TO v DO INSTEAD INSERT INTO t VALUES (new.a) RETURNING a::text;",
            "RETURNING list's entry 1 has different type from column \"a\"",
        ),
        (
            "CREATE RULE x AS ON INSERT TO v DO INSTEAD INSERT INTO t VALUES (new.a) RETURNING a, a;",
            "RETURNING list has too many entries",
        ),
        (
            "CREATE RULE \"_RETURN\" AS ON INSERT TO t DO INSTEAD NOTHING;",
            "non-view rule for \"t\" must not be named \"_RETURN\"",
        ),
        (
            "CREATE RULE \"_RETURN\" AS ON SELECT TO v DO INSTEAD SELECT a FROM t;",
            "\"v\" is already a view",
        ),
        (
            "CREATE OR REPLACE RULE \"_RETURN\" AS ON SELECT TO v DO INSTEAD SELECT a::bigint AS a FROM u;",
            "SELECT rule's target entry 1 has different type from column \"a\"",
        ),
        (
            "CREATE OR REPLACE RULE \"_RETURN\" AS ON SELECT TO v DO INSTEAD SELECT a AS b FROM u;",
            "SELECT rule's target entry 1 has different column name from column \"a\"",
        ),
        (
            "CREATE OR REPLACE RULE x AS ON SELECT TO v DO INSTEAD SELECT a FROM u;",
            "view rule for \"v\" must be named \"_RETURN\"",
        ),
        (
            "CREATE OR REPLACE RULE \"_RETURN\" AS ON SELECT TO v DO INSTEAD SELECT a FROM u;
             DROP TABLE u;",
            "cannot drop table u because other objects depend on it",
        ),
        (
            "ALTER RULE \"_RETURN\" ON v RENAME TO x;",
            "renaming an ON SELECT rule is not allowed",
        ),
        (
            "ALTER RULE r ON t RENAME TO \"_RETURN\";",
            "non-view rule for \"t\" must not be named \"_RETURN\"",
        ),
        (
            "DROP RULE \"_RETURN\" ON v;",
            "cannot drop rule _RETURN on view v because view v requires it",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE RULE x AS ON INSERT TO v DO INSTEAD INSERT INTO t VALUES (new.a) RETURNING *;
             CREATE RULE y AS ON UPDATE TO v DO INSTEAD
                 (UPDATE t SET a = new.a; UPDATE u SET a = new.a RETURNING a);
             COMMENT ON RULE \"_RETURN\" ON v IS 'x';
             CREATE OR REPLACE RULE \"_RETURN\" AS ON SELECT TO v DO INSTEAD SELECT a FROM u;
             DROP TABLE t CASCADE;",
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
fn transaction_control_follows_the_transaction_block() {
    // PG 18 xact.c (DefineSavepoint, ReleaseSavepoint, RollbackToSavepoint,
    // EndTransactionBlock, PrepareTransactionBlock), PreventCommandIfReadOnly
    // / ExecCheckXactReadOnly, and AfterTriggerSetState (SET CONSTRAINTS).
    let setup = "CREATE TABLE t (a int UNIQUE);
                 CREATE TABLE d (a int UNIQUE DEFERRABLE);";
    for (stmt, msg) in [
        (
            "SAVEPOINT sp;",
            "SAVEPOINT can only be used in transaction blocks",
        ),
        (
            "SELECT 1; SAVEPOINT sp;",
            "SAVEPOINT can only be used in transaction blocks",
        ),
        (
            "SELECT 1; RELEASE SAVEPOINT sp;",
            "RELEASE SAVEPOINT can only be used in transaction blocks",
        ),
        (
            "BEGIN; ROLLBACK TO SAVEPOINT nosp; COMMIT;",
            "savepoint \"nosp\" does not exist",
        ),
        (
            "BEGIN; SAVEPOINT a; RELEASE SAVEPOINT a; RELEASE SAVEPOINT a; COMMIT;",
            "savepoint \"a\" does not exist",
        ),
        (
            "BEGIN; DISCARD ALL; COMMIT;",
            "DISCARD ALL cannot run inside a transaction block",
        ),
        (
            "BEGIN; VACUUM t; COMMIT;",
            "VACUUM cannot run inside a transaction block",
        ),
        (
            "BEGIN READ ONLY; CREATE TABLE u (a int); COMMIT;",
            "cannot execute CREATE TABLE in a read-only transaction",
        ),
        (
            "BEGIN READ ONLY; CREATE TEMP TABLE u (a int); COMMIT;",
            "cannot execute CREATE TABLE in a read-only transaction",
        ),
        (
            "BEGIN READ ONLY; INSERT INTO t VALUES (1); COMMIT;",
            "cannot execute INSERT in a read-only transaction",
        ),
        (
            "SET TRANSACTION READ ONLY; COMMENT ON TABLE t IS 'x';",
            "cannot execute COMMENT in a read-only transaction",
        ),
        (
            "BEGIN READ ONLY; SELECT 1; SET TRANSACTION READ WRITE; COMMIT;",
            "transaction read-write mode must be set before any query",
        ),
        (
            "SET CONSTRAINTS nosuch IMMEDIATE;",
            "constraint \"nosuch\" does not exist",
        ),
        (
            "SET CONSTRAINTS nosch.t_a_key IMMEDIATE;",
            "schema \"nosch\" does not exist",
        ),
        (
            "BEGIN; SET CONSTRAINTS t_a_key DEFERRED; COMMIT;",
            "constraint \"t_a_key\" is not deferrable",
        ),
        (
            "SELECT 1; COMMIT AND CHAIN;",
            "COMMIT AND CHAIN can only be used in transaction blocks",
        ),
        (
            "BEGIN; CREATE TABLE u (a int); PREPARE TRANSACTION 'x';",
            "prepared transactions are disabled",
        ),
        (
            "COMMIT PREPARED 'x';",
            "prepared transaction with identifier \"x\" does not exist",
        ),
        (
            "SELECT 1; ROLLBACK PREPARED 'x';",
            "ROLLBACK PREPARED cannot run inside a transaction block",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    for ok in [
        "BEGIN; SAVEPOINT a; SAVEPOINT a; RELEASE SAVEPOINT a; RELEASE SAVEPOINT a; COMMIT;",
        "BEGIN READ ONLY; SELECT a FROM t; SET TRANSACTION READ ONLY; COMMIT;",
        "BEGIN READ ONLY; SET transaction_read_only = off; CREATE TABLE u (a int); COMMIT;",
        "BEGIN; SET CONSTRAINTS d_a_key, t_a_key IMMEDIATE; SET CONSTRAINTS d_a_key DEFERRED; COMMIT;",
        "SET CONSTRAINTS ALL DEFERRED;",
        "SET TRANSACTION READ ONLY;",
        "PREPARE TRANSACTION 'x';",
        "BEGIN; CREATE TABLE u (a int); COMMIT AND CHAIN; CREATE TABLE w (a int); COMMIT;",
    ] {
        build_db(&[("0001.sql", setup), ("0002.sql", ok)]);
    }
}

#[test]
fn rollback_undoes_the_catalog_changes_of_its_transaction() {
    // PG 18: ROLLBACK and ROLLBACK TO SAVEPOINT undo everything since the
    // transaction / savepoint started — catalog changes and settings alike.
    let db = build_db(&[
        ("0001.sql", "BEGIN; CREATE TABLE t (a int); ROLLBACK;"),
        ("0002.sql", "CREATE TABLE t (b int);"),
        (
            "0003.sql",
            "BEGIN; SET search_path = pg_catalog; ROLLBACK; CREATE TABLE u (a int);",
        ),
        (
            "0004.sql",
            "BEGIN;
             CREATE TABLE v (a int);
             SAVEPOINT sp;
             CREATE TABLE w (a int);
             SET LOCAL search_path = pg_catalog;
             ROLLBACK TO SAVEPOINT sp;
             CREATE TABLE w (b int);
             COMMIT;",
        ),
        // ROLLBACK in the implicit block of a multi-statement query aborts
        // it too.
        (
            "0005.sql",
            "CREATE TABLE x (a int); ROLLBACK; CREATE TABLE x (b int);",
        ),
    ]);
    for sql in [
        "SELECT b FROM t",
        "SELECT a FROM public.u",
        "SELECT a FROM v",
        "SELECT b FROM public.w",
        "SELECT b FROM x",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn a_failed_migration_leaves_no_trace() {
    // PG 18: the failing statement aborts the migration's transaction, and
    // with it every statement since the last COMMIT.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (a int); CREATE TABLE t (a int);")
        .expect_err("duplicate table");
    let err = db
        .analyze("SELECT a FROM t")
        .expect_err("t was rolled back");
    assert!(
        err.to_string().starts_with("relation \"t\" does not exist"),
        "got: {err}"
    );
    // What a COMMIT made durable stays.
    db.apply_sql("CREATE TABLE c (a int); COMMIT; CREATE TABLE d (a int); CREATE TABLE c (a int);")
        .expect_err("duplicate table");
    db.analyze("SELECT a FROM c").unwrap();
    let err = db
        .analyze("SELECT a FROM d")
        .expect_err("d was rolled back");
    assert!(
        err.to_string().starts_with("relation \"d\" does not exist"),
        "got: {err}"
    );
}

#[test]
fn migrations_the_runner_wraps_in_a_transaction() {
    // With `use_transaction`, the runner runs each migration inside a
    // transaction block of its own: savepoints work, and statements that
    // can't run in a block need `-- no-transaction`. (The sanity mirror
    // sends migrations unwrapped.)
    let wrapped = |sql: &str| {
        let mut db = PgCatalog::new().unwrap();
        db.skip_pg_sanity();
        db.set_migrations_use_transaction(true);
        db.apply_sql("CREATE TABLE t (a int);").unwrap();
        db.apply_sql(sql).map(|()| db)
    };
    let db = wrapped("SAVEPOINT sp; CREATE TABLE u (a int); ROLLBACK TO SAVEPOINT sp;").unwrap();
    assert!(db.resolve_table(None, "u").is_none());
    wrapped("SAVEPOINT sp;").unwrap();
    wrapped("LOCK TABLE t;").unwrap();
    wrapped("-- no-transaction\nCREATE INDEX CONCURRENTLY ON t (a);").unwrap();
    let Err(err) = wrapped("CREATE INDEX CONCURRENTLY ON t (a);") else {
        panic!("CREATE INDEX CONCURRENTLY ran in the runner's transaction");
    };
    assert!(
        err.to_string()
            .starts_with("CREATE INDEX CONCURRENTLY cannot run inside a transaction block"),
        "got: {err}"
    );
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

#[test]
fn configuration_parameters_are_validated() {
    // PG 18 set_config_with_handle / parse_and_validate_value /
    // assignable_custom_variable_name.
    let setup = "CREATE TABLE t (a int);";
    for (stmt, msg) in [
        (
            "SET nosuch_param = 1;",
            "unrecognized configuration parameter \"nosuch_param\"",
        ),
        (
            "RESET nosuch_param;",
            "unrecognized configuration parameter \"nosuch_param\"",
        ),
        (
            "SET LOCAL nosuch = 1;",
            "unrecognized configuration parameter \"nosuch\"",
        ),
        (
            "SELECT set_config('nosuch', '1', false);",
            "unrecognized configuration parameter \"nosuch\"",
        ),
        (
            "SET statement_timeout = 'abc';",
            "invalid value for parameter \"statement_timeout\": \"abc\"",
        ),
        (
            "SET statement_timeout = -5;",
            "-5 ms is outside the valid range for parameter \"statement_timeout\" (0 ms .. 2147483647 ms)",
        ),
        (
            "SET enable_seqscan = maybe;",
            "parameter \"enable_seqscan\" requires a Boolean value",
        ),
        (
            "SET client_min_messages = loud;",
            "invalid value for parameter \"client_min_messages\": \"loud\"",
        ),
        (
            "SET shared_buffers = '1GB';",
            "parameter \"shared_buffers\" cannot be changed without restarting the server",
        ),
        (
            "SET wal_level = minimal;",
            "parameter \"wal_level\" cannot be changed without restarting the server",
        ),
        (
            "SET default_table_access_method = nosuch;",
            "invalid value for parameter \"default_table_access_method\": \"nosuch\"",
        ),
        (
            "SET seq_page_cost = -1;",
            "-1 is outside the valid range for parameter \"seq_page_cost\" (0 .. 1.79769e+308)",
        ),
        (
            "SET work_mem = 32;",
            "32 kB is outside the valid range for parameter \"work_mem\" (64 kB .. 2147483647 kB)",
        ),
        (
            "SET work_mem = '10 parsecs';",
            "invalid value for parameter \"work_mem\": \"10 parsecs\"",
        ),
        (
            "SET work_mem = '5s';",
            "invalid value for parameter \"work_mem\": \"5s\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    // What pg_dump emits, and other common settings.
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "SET statement_timeout = 0;
             SET lock_timeout = 0;
             SET idle_in_transaction_session_timeout = 0;
             SET transaction_timeout = 0;
             SET client_encoding = 'UTF8';
             SET standard_conforming_strings = on;
             SELECT pg_catalog.set_config('search_path', '', false);
             SET check_function_bodies = false;
             SET xmloption = content;
             SET client_min_messages = warning;
             SET row_security = off;
             SET default_tablespace = '';
             SET default_table_access_method = heap;
             SET app.custom = 'x';
             SET statement_timeout = '5s';
             SET work_mem = '1TB';
             SET work_mem = '64MB';
             SET TIME ZONE 'UTC';
             SET timezone = 'UTC';
             SET datestyle = 'ISO, MDY';
             SET session_replication_role = replica;
             SET constraint_exclusion = true;
             SET client_min_messages = info;
             SET LOCAL lock_timeout = '10s';
             RESET lock_timeout;
             SET enable_seqscan TO DEFAULT;
             SET search_path TO public;
             RESET ALL;",
        ),
    ]);
}

#[test]
fn drop_index_refuses_constraint_indexes() {
    // PG 18 findDependentObjects: a constraint's index goes only with the
    // constraint.
    let setup = "CREATE TABLE t (a int PRIMARY KEY, b int UNIQUE, c int);
                 CREATE UNIQUE INDEX tc ON t (c);";
    for (stmt, msg) in [
        (
            "DROP INDEX t_pkey;",
            "cannot drop index t_pkey because constraint t_pkey on table t requires it",
        ),
        (
            "DROP INDEX t_b_key;",
            "cannot drop index t_b_key because constraint t_b_key on table t requires it",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT tc UNIQUE USING INDEX tc; DROP INDEX tc;",
            "cannot drop index tc because constraint tc on table t requires it",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "DROP INDEX tc;
             ALTER TABLE t DROP CONSTRAINT t_b_key;
             ALTER TABLE t DROP CONSTRAINT t_pkey;",
        ),
    ]);
}

#[test]
fn partition_keys_are_validated() {
    // PG 18 transformCreateStmt / transformPartitionSpec /
    // ComputePartitionAttrs.
    let setup = "CREATE TABLE t (a int);";
    for (stmt, msg) in [
        (
            "CREATE TABLE u (a int) INHERITS (t) PARTITION BY LIST (a);",
            "cannot create partitioned table as inheritance child",
        ),
        (
            "CREATE TABLE p (a int, b int) PARTITION BY LIST (a, b);",
            "cannot use \"list\" partition strategy with more than one column",
        ),
        (
            "CREATE TABLE p (a int) PARTITION BY RANGE ((1));",
            "cannot use constant expression as partition key",
        ),
        (
            "CREATE TABLE p (j json) PARTITION BY RANGE (j);",
            "data type json has no default operator class for access method \"btree\"",
        ),
        (
            "CREATE TABLE p (j json) PARTITION BY HASH (j);",
            "data type json has no default operator class for access method \"hash\"",
        ),
        (
            "CREATE TABLE p (a int) PARTITION BY RANGE (ctid);",
            "cannot use system column \"ctid\" in partition key",
        ),
        (
            "CREATE TABLE p (a int, g int GENERATED ALWAYS AS (a * 2) STORED) PARTITION BY RANGE (g);",
            "cannot use generated column in partition key",
        ),
        (
            "CREATE TABLE p (a int) PARTITION BY RANGE (a text_ops);",
            "operator class \"text_ops\" does not accept data type integer",
        ),
        (
            "CREATE TABLE p (a int) PARTITION BY RANGE ((a + random()::int));",
            "functions in partition key expression must be marked IMMUTABLE",
        ),
        (
            "CREATE TABLE p (a int) PARTITION BY RANGE ((sum(a)));",
            "aggregate functions are not allowed in partition key expressions",
        ),
        (
            "CREATE TABLE p (a int) PARTITION BY RANGE ((a + nosuch));",
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
            "CREATE TABLE p7 (a int) PARTITION BY RANGE (a int4_ops);
             CREATE TABLE p11 (a text) PARTITION BY RANGE (a COLLATE \"C\");
             CREATE TABLE p12 (a int) PARTITION BY RANGE (a, a);
             CREATE TABLE p14 (a int[]) PARTITION BY LIST (a);
             CREATE TABLE p15 (a int) PARTITION BY LIST ((a > 0));
             CREATE TABLE p16 (a int, b int) PARTITION BY LIST (a);
             CREATE TABLE p17 PARTITION OF p16 FOR VALUES IN (1) PARTITION BY RANGE (b);",
        ),
    ]);
}

#[test]
fn sequence_parameters_are_validated() {
    // PG 18 init_params (sequence.c).
    let setup = "CREATE SEQUENCE s10 AS int;
                 CREATE TABLE t (a smallserial, b int);";
    for (stmt, msg) in [
        (
            "CREATE SEQUENCE s MAXVALUE 5 START 10;",
            "START value (10) cannot be greater than MAXVALUE (5)",
        ),
        (
            "CREATE SEQUENCE s AS text;",
            "sequence type must be smallint, integer, or bigint",
        ),
        (
            "CREATE SEQUENCE s AS nosuch;",
            "type \"nosuch\" does not exist",
        ),
        (
            "CREATE SEQUENCE s INCREMENT 0;",
            "INCREMENT must not be zero",
        ),
        (
            "CREATE SEQUENCE s MINVALUE 10 MAXVALUE 5;",
            "MINVALUE (10) must be less than MAXVALUE (5)",
        ),
        (
            "CREATE SEQUENCE s CACHE 0;",
            "CACHE (0) must be greater than zero",
        ),
        (
            "CREATE SEQUENCE s START 0;",
            "START value (0) cannot be less than MINVALUE (1)",
        ),
        (
            "CREATE SEQUENCE s AS smallint MAXVALUE 100000;",
            "MAXVALUE (100000) is out of range for sequence data type smallint",
        ),
        (
            "CREATE SEQUENCE s INCREMENT 1 INCREMENT 2;",
            "conflicting or redundant options",
        ),
        (
            "ALTER SEQUENCE s10 MAXVALUE 5 RESTART 10;",
            "RESTART value (10) cannot be greater than MAXVALUE (5)",
        ),
        (
            "CREATE SEQUENCE s INCREMENT -1 START 5;",
            "START value (5) cannot be greater than MAXVALUE (-1)",
        ),
        (
            "ALTER SEQUENCE t_a_seq MAXVALUE 100000;",
            "MAXVALUE (100000) is out of range for sequence data type smallint",
        ),
        ("ALTER SEQUENCE t MAXVALUE 5;", "cannot open relation \"t\""),
        (
            "ALTER SEQUENCE s10 START 0;",
            "START value (0) cannot be less than MINVALUE (1)",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER SEQUENCE s10 AS smallint;
             ALTER SEQUENCE s10 MAXVALUE 3;
             CREATE SEQUENCE s11 INCREMENT -1;
             CREATE SEQUENCE s12 AS bigint MAXVALUE 9223372036854775807;
             CREATE SEQUENCE s13 MINVALUE -10 START -5 CACHE 20 CYCLE;
             ALTER SEQUENCE s13 RESTART;
             ALTER SEQUENCE s13 NO MAXVALUE;
             ALTER SEQUENCE t_a_seq MAXVALUE 1000;",
        ),
    ]);
}

#[test]
fn foreign_data_wrappers_servers_and_user_mappings() {
    // PG 18 foreigncmds.c / get_foreign_server_oid.
    let setup = "CREATE FOREIGN DATA WRAPPER w;
                 CREATE SERVER s FOREIGN DATA WRAPPER w;
                 CREATE USER MAPPING FOR public SERVER s;
                 CREATE FOREIGN TABLE ft (a int) SERVER s;";
    for (stmt, msg) in [
        (
            "CREATE FOREIGN DATA WRAPPER w;",
            "foreign-data wrapper \"w\" already exists",
        ),
        (
            "CREATE FOREIGN DATA WRAPPER w2 HANDLER nosuch;",
            "function nosuch() does not exist",
        ),
        (
            "CREATE FOREIGN DATA WRAPPER w3 VALIDATOR nosuch;",
            "function nosuch(text[], oid) does not exist",
        ),
        (
            "CREATE SERVER s2 FOREIGN DATA WRAPPER nosuch;",
            "foreign-data wrapper \"nosuch\" does not exist",
        ),
        (
            "CREATE SERVER s FOREIGN DATA WRAPPER w;",
            "server \"s\" already exists",
        ),
        (
            "CREATE USER MAPPING FOR public SERVER nosuch;",
            "server \"nosuch\" does not exist",
        ),
        (
            "CREATE USER MAPPING FOR public SERVER s;",
            "user mapping for \"public\" already exists for server \"s\"",
        ),
        (
            "CREATE FOREIGN TABLE ft2 (a int) SERVER nosuch;",
            "server \"nosuch\" does not exist",
        ),
        ("DROP SERVER nosuch;", "server \"nosuch\" does not exist"),
        (
            "DROP SERVER s;",
            "cannot drop server s because other objects depend on it",
        ),
        (
            "DROP FOREIGN DATA WRAPPER nosuch;",
            "foreign-data wrapper \"nosuch\" does not exist",
        ),
        (
            "DROP FOREIGN DATA WRAPPER w;",
            "cannot drop foreign-data wrapper w because other objects depend on it",
        ),
        (
            "ALTER SERVER nosuch OPTIONS (a 'b');",
            "server \"nosuch\" does not exist",
        ),
        (
            "ALTER FOREIGN DATA WRAPPER nosuch OPTIONS (a 'b');",
            "foreign-data wrapper \"nosuch\" does not exist",
        ),
        (
            "DROP USER MAPPING FOR public SERVER nosuch;",
            "server \"nosuch\" does not exist",
        ),
        (
            "DROP USER MAPPING FOR public SERVER s; DROP USER MAPPING FOR public SERVER s;",
            "user mapping for \"public\" does not exist for server \"s\"",
        ),
        (
            "IMPORT FOREIGN SCHEMA x FROM SERVER s INTO nosuch;",
            "schema \"nosuch\" does not exist",
        ),
        (
            "IMPORT FOREIGN SCHEMA x FROM SERVER nosuch INTO public;",
            "server \"nosuch\" does not exist",
        ),
        (
            "ALTER SERVER nosuch RENAME TO s3;",
            "server \"nosuch\" does not exist",
        ),
        (
            "DROP SERVER s CASCADE; SELECT * FROM ft;",
            "relation \"ft\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE SERVER IF NOT EXISTS s FOREIGN DATA WRAPPER w;
             ALTER SERVER s RENAME TO s2;
             CREATE FOREIGN TABLE ft3 (a int) SERVER s2;
             ALTER SERVER s2 OPTIONS (ADD host 'x');
             DROP USER MAPPING IF EXISTS FOR public SERVER nosuch;
             DROP USER MAPPING FOR public SERVER s2;
             DROP SERVER IF EXISTS nosuch;
             DROP FOREIGN DATA WRAPPER w CASCADE;
             CREATE FOREIGN DATA WRAPPER w;",
        ),
    ]);
}

#[test]
fn new_objects_need_an_existing_schema() {
    // PG 18 RangeVarGetCreationNamespace / QualifiedNameGetCreationNamespace:
    // an object created in a named schema needs that schema.
    let setup = "CREATE TABLE t (a int);";
    for stmt in [
        "CREATE TABLE nosuch.t2 (a int);",
        "CREATE VIEW nosuch.v AS SELECT 1;",
        "CREATE SEQUENCE nosuch.s;",
        "CREATE FUNCTION nosuch.f() RETURNS int LANGUAGE sql AS 'select 1';",
        "CREATE TYPE nosuch.e AS ENUM ('a');",
        "CREATE DOMAIN nosuch.d AS int;",
        "CREATE AGGREGATE nosuch.a (int) (sfunc = int4pl, stype = int);",
        "CREATE COLLATION nosuch.c FROM \"C\";",
        "CREATE OPERATOR nosuch.### (leftarg = int, rightarg = int, function = int4pl);",
        "ALTER TABLE t SET SCHEMA nosuch;",
        "CREATE TYPE nosuch.r AS RANGE (subtype = int4);",
        "CREATE TYPE nosuch.c AS (a int);",
        "CREATE MATERIALIZED VIEW nosuch.mv AS SELECT 1;",
        "CREATE TABLE nosuch.ct AS SELECT 1;",
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(
            err.to_string()
                .starts_with("schema \"nosuch\" does not exist"),
            "{stmt}\n  got: {err}"
        );
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE SCHEMA s;
             CREATE TABLE s.t2 (a int);
             CREATE FUNCTION s.f() RETURNS int LANGUAGE sql AS 'select 1';
             ALTER TABLE t SET SCHEMA s;",
        ),
    ]);
}

#[test]
fn extensions_follow_create_extension_rules() {
    // PG 18 CreateExtensionInternal / get_required_extension /
    // ExecAlterExtensionStmt / ExecAlterExtensionContentsRecurse /
    // AlterExtensionNamespace.
    let setup = "CREATE TABLE t (a int); CREATE SCHEMA x;";
    for (stmt, msg) in [
        (
            "CREATE EXTENSION citext SCHEMA nosuch;",
            "schema \"nosuch\" does not exist",
        ),
        (
            "CREATE EXTENSION citext VERSION '9.9';",
            "extension \"citext\" has no installation script nor update path for version \"9.9\"",
        ),
        (
            "CREATE EXTENSION citext; ALTER EXTENSION citext UPDATE TO '9.9';",
            "extension \"citext\" has no update path from version \"1.8\" to version \"9.9\"",
        ),
        (
            "ALTER EXTENSION nosuch UPDATE;",
            "extension \"nosuch\" does not exist",
        ),
        (
            "ALTER EXTENSION nosuch ADD TABLE t;",
            "extension \"nosuch\" does not exist",
        ),
        (
            "CREATE EXTENSION citext; ALTER EXTENSION citext ADD TABLE nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE EXTENSION citext; ALTER EXTENSION citext ADD TABLE t; ALTER EXTENSION citext ADD TABLE t;",
            "table t is already a member of extension \"citext\"",
        ),
        (
            "CREATE EXTENSION citext; ALTER EXTENSION citext DROP TABLE t;",
            "table t is not a member of extension \"citext\"",
        ),
        (
            "CREATE EXTENSION citext; ALTER EXTENSION citext SET SCHEMA nosuch;",
            "schema \"nosuch\" does not exist",
        ),
        (
            "CREATE EXTENSION earthdistance;",
            "required extension \"cube\" is not installed",
        ),
        (
            "CREATE SCHEMA s; SET search_path = s; CREATE EXTENSION citext; SELECT 'x'::public.citext;",
            "type \"public.citext\" does not exist",
        ),
        (
            "CREATE EXTENSION cube; ALTER EXTENSION cube SET SCHEMA x; SELECT '(1)'::public.cube;",
            "type \"public.cube\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE EXTENSION earthdistance CASCADE;
             CREATE EXTENSION citext SCHEMA x;
             SELECT 'a'::x.citext;
             ALTER EXTENSION citext ADD TABLE t;
             ALTER EXTENSION citext DROP TABLE t;
             ALTER EXTENSION cube SET SCHEMA x;
             SELECT '(1)'::x.cube;
             CREATE EXTENSION IF NOT EXISTS citext;",
        ),
    ]);
}

#[test]
fn do_blocks_are_compiled() {
    // PG 18 ExecuteDoStmt / plpgsql_inline_handler compiles the block.
    let setup = "CREATE TABLE t (a int);";
    for (stmt, msg) in [
        (
            "DO $$ BEGIN PERFORM 1; END $$ LANGUAGE sql;",
            "language \"sql\" does not support inline code execution",
        ),
        (
            "DO $$ BEGIN PERFORM 1; END $$ LANGUAGE nosuch;",
            "language \"nosuch\" does not exist",
        ),
        (
            "DO $$ DECLARE x nosuchtype; BEGIN PERFORM 1; END $$;",
            "type \"nosuchtype\" does not exist",
        ),
        (
            "DO $$ BEGIN RAISE NOTICE 'x' $$;",
            "syntax error at end of input",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "DO $$ BEGIN PERFORM 1; END $$;
             DO LANGUAGE plpgsql $$ DECLARE r t%ROWTYPE; n int := 0; BEGIN
               FOR r IN SELECT * FROM t LOOP n := n + 1; END LOOP;
               RAISE NOTICE 'rows: %', n;
             END $$;
             DO $body$ BEGIN IF true THEN RAISE NOTICE '$$'; END IF; END $body$;",
        ),
    ]);
}

#[test]
fn do_blocks_run_their_straight_line_statements() {
    // PG 18 plpgsql_inline_handler runs the block: its SQL statements,
    // EXECUTE of a string, RAISE, assignments (exec_stmt_block,
    // exec_stmt_execsql, exec_stmt_dynexecute, exec_stmt_raise,
    // exec_stmt_assign), nested blocks and their exception handlers.
    let setup = "CREATE TABLE t (a int);";
    for (stmt, msg) in [
        ("DO $$ BEGIN RAISE EXCEPTION 'boom'; END $$;", "boom"),
        (
            "DO $$ BEGIN RAISE 'boom % and %', 1, 'two'; END $$;",
            "boom 1 and two",
        ),
        (
            "DO $$ DECLARE n int := 7; BEGIN RAISE EXCEPTION 'n is %, 100%%', n; END $$;",
            "n is 7, 100%",
        ),
        (
            "DO $$ BEGIN RAISE EXCEPTION USING MESSAGE = 'via using'; END $$;",
            "via using",
        ),
        (
            "DO $$ BEGIN RAISE division_by_zero; END $$;",
            "division_by_zero",
        ),
        (
            "DO $$ DECLARE x int; BEGIN x := 'abc'; END $$;",
            "invalid input syntax for type integer: \"abc\"",
        ),
        (
            "DO $$ DECLARE x int := 'abc'; BEGIN NULL; END $$;",
            "invalid input syntax for type integer: \"abc\"",
        ),
        (
            "DO $$ BEGIN CREATE TABLE t (a int); END $$;",
            "relation \"t\" already exists",
        ),
        (
            "DO $$ BEGIN INSERT INTO nosuch VALUES (1); END $$;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "DO $$ BEGIN CREATE TABLE u (a int); END $$; SELECT b FROM u;",
            "column \"b\" does not exist",
        ),
        (
            "DO $$ BEGIN EXECUTE 'CREATE TABLE u (a int)'; END $$; CREATE TABLE u (a int);",
            "relation \"u\" already exists",
        ),
        (
            "DO $$ BEGIN
               BEGIN CREATE TABLE t (a int); EXCEPTION WHEN others THEN NULL; END;
               CREATE TABLE u (a int);
               RAISE EXCEPTION 'after';
             END $$;",
            "after",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "DO $$ BEGIN CREATE TABLE u (a int); END $$;
             DO $$ BEGIN EXECUTE 'CREATE TABLE w (b text)'; END $$;
             DO $$ DECLARE n int := 1; BEGIN
               n := 2;
               RAISE NOTICE 'n = %', n;
               CREATE VIEW v AS SELECT a FROM u;
             END $$;
             DO $$ BEGIN
               BEGIN CREATE TABLE x (a int); CREATE TABLE t (a int);
               EXCEPTION WHEN others THEN CREATE TABLE y (c int);
               END;
             END $$;
             DO $$ BEGIN RETURN; RAISE EXCEPTION 'unreached'; END $$;",
        ),
    ]);
    for sql in [
        "SELECT a FROM u",
        "SELECT b FROM w",
        "SELECT a FROM v",
        "SELECT c FROM y",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // The failing block's subtransaction rolled x back.
    assert!(db.resolve_table(None, "x").is_none());
}

#[test]
fn copy_resolves_its_relation_and_options() {
    // PG 18 DoCopy / ProcessCopyOptions / BeginCopyTo / BeginCopyFrom.
    let setup = "CREATE TABLE t (a int, b int);
                 CREATE VIEW v AS SELECT 1 AS a;
                 CREATE SEQUENCE s;
                 CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a;
                 CREATE TABLE p (a int) PARTITION BY LIST (a);";
    for (stmt, msg) in [
        ("COPY v TO STDOUT;", "cannot copy from view \"v\""),
        ("COPY s TO STDOUT;", "cannot copy from sequence \"s\""),
        (
            "COPY p TO STDOUT;",
            "cannot copy from partitioned table \"p\"",
        ),
        (
            "COPY t (a, a) TO STDOUT;",
            "column \"a\" specified more than once",
        ),
        (
            "COPY t (nosuch) TO STDOUT;",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "COPY nosuch TO STDOUT;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "COPY (SELECT nosuch FROM t) TO STDOUT;",
            "column \"nosuch\" does not exist",
        ),
        (
            "COPY t TO STDOUT (FORMAT nosuch);",
            "COPY format \"nosuch\" not recognized",
        ),
        (
            "COPY t TO STDOUT (nosuchopt 1);",
            "option \"nosuchopt\" not recognized",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "COPY t TO STDOUT;
             COPY t (a) TO STDOUT (FORMAT csv, HEADER);
             COPY mv TO STDOUT;
             COPY (SELECT * FROM v) TO STDOUT;",
        ),
    ]);
}

#[test]
fn new_enum_labels_are_unusable_until_committed() {
    // PG 18 check_safe_enum_use: a label added by ALTER TYPE ... ADD VALUE
    // can't be used in the same transaction, unless the type is new too.
    let setup = "CREATE TYPE mood AS ENUM ('a');";
    for (stmt, msg) in [
        (
            "ALTER TYPE mood ADD VALUE 'b'; SELECT 'b'::mood;",
            "unsafe use of new value \"b\" of enum type mood",
        ),
        (
            "ALTER TYPE mood ADD VALUE 'b'; CREATE TABLE u (m mood DEFAULT 'b');",
            "unsafe use of new value \"b\" of enum type mood",
        ),
        (
            "ALTER TYPE mood ADD VALUE 'b'; ALTER TYPE mood RENAME VALUE 'b' TO 'c'; SELECT 'c'::mood;",
            "unsafe use of new value \"c\" of enum type mood",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TYPE mood ADD VALUE 'b'; SELECT 'a'::mood;",
        ),
        ("0003.sql", "SELECT 'b'::mood;"),
        (
            "0004.sql",
            "CREATE TYPE mood2 AS ENUM ('x'); ALTER TYPE mood2 ADD VALUE 'y'; SELECT 'y'::mood2;",
        ),
    ]);
}

#[test]
fn check_option_needs_an_auto_updatable_view() {
    // PG 18 DefineView / ATExecSetRelOptions / view_query_is_auto_updatable.
    let setup = "CREATE TABLE t (a int, b int);
                 CREATE VIEW v12 AS SELECT a FROM t;
                 CREATE VIEW v13 AS SELECT a + 1 AS x FROM t;";
    let reason = |r: &str| {
        format!("WITH CHECK OPTION is supported only on automatically updatable views ({r}")
    };
    for (stmt, msg) in [
        (
            "CREATE VIEW v AS SELECT DISTINCT a FROM t WITH CHECK OPTION;",
            reason("Views containing DISTINCT"),
        ),
        (
            "CREATE VIEW v AS SELECT a FROM t GROUP BY a WITH CHECK OPTION;",
            reason("Views containing GROUP BY"),
        ),
        (
            "CREATE VIEW v AS SELECT a + 1 AS x FROM t WITH CHECK OPTION;",
            reason("Views that have no updatable columns"),
        ),
        (
            "CREATE VIEW v AS SELECT 1 AS x WITH CHECK OPTION;",
            reason("Views that do not select from a single table or view"),
        ),
        (
            "CREATE VIEW v AS SELECT a, generate_series(1, 2) FROM t WITH CHECK OPTION;",
            reason("Views that return set-returning functions"),
        ),
        (
            "CREATE VIEW v AS SELECT a FROM t LIMIT 1 WITH CHECK OPTION;",
            reason("Views containing LIMIT or OFFSET"),
        ),
        (
            "CREATE VIEW v AS SELECT a FROM t UNION SELECT b FROM t WITH CHECK OPTION;",
            reason("Views containing UNION, INTERSECT, or EXCEPT"),
        ),
        (
            "CREATE VIEW v AS SELECT count(*) FROM t WITH CHECK OPTION;",
            reason("Views that return aggregate functions"),
        ),
        (
            "CREATE VIEW v AS SELECT t.a FROM t, t u WITH CHECK OPTION;",
            reason("Views that do not select from a single table or view"),
        ),
        (
            "ALTER VIEW v13 SET (check_option = local);",
            reason("Views that have no updatable columns"),
        ),
        (
            "CREATE VIEW v WITH (check_option = sometimes) AS SELECT a FROM t;",
            "invalid value for enum option \"check_option\": sometimes".to_owned(),
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(&msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE VIEW v6 AS SELECT a FROM t WHERE a > 0 WITH CASCADED CHECK OPTION;
             CREATE VIEW v7 AS SELECT * FROM v6 WITH LOCAL CHECK OPTION;
             CREATE VIEW v15 AS SELECT a, a + 1 AS y FROM t WITH CHECK OPTION;
             ALTER VIEW v12 SET (check_option = local);
             ALTER VIEW v13 RESET (check_option);",
        ),
    ]);
}

#[test]
fn publications_are_validated_and_tracked() {
    // PG 18 CreatePublication / AlterPublication / parse_publication_options
    // / check_publication_add_relation.
    let setup = "CREATE TABLE t (a int PRIMARY KEY, b int);
                 CREATE TABLE u (x int);
                 CREATE VIEW v AS SELECT 1 AS a;
                 CREATE PUBLICATION p FOR TABLE t;";
    for (stmt, msg) in [
        (
            "CREATE PUBLICATION q FOR TABLE nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE v;",
            "cannot add relation \"v\" to publication",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE t (nosuch);",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "CREATE PUBLICATION q FOR TABLES IN SCHEMA nosuch;",
            "schema \"nosuch\" does not exist",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE t WHERE (nosuch > 0);",
            "column \"nosuch\" does not exist",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE t WITH (publish = 'bogus');",
            "unrecognized value for publication option \"publish\": \"bogus\"",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE t WITH (nosuch = 1);",
            "unrecognized publication parameter: \"nosuch\"",
        ),
        (
            "CREATE PUBLICATION p FOR TABLE u;",
            "publication \"p\" already exists",
        ),
        (
            "ALTER PUBLICATION nosuch ADD TABLE t;",
            "publication \"nosuch\" does not exist",
        ),
        (
            "ALTER PUBLICATION p ADD TABLE t;",
            "relation \"t\" is already member of publication \"p\"",
        ),
        (
            "ALTER PUBLICATION p DROP TABLE nosuch;",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER PUBLICATION p DROP TABLE u;",
            "relation \"u\" is not part of the publication",
        ),
        (
            "DROP PUBLICATION nosuch;",
            "publication \"nosuch\" does not exist",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE t, nosuch;",
            "relation \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE PUBLICATION q FOR TABLE t (a, b) WHERE (a > 0), u WITH (publish = 'insert, update');
             CREATE PUBLICATION r FOR TABLES IN SCHEMA public;
             CREATE PUBLICATION s FOR ALL TABLES;
             ALTER PUBLICATION p ADD TABLE u;
             ALTER PUBLICATION p DROP TABLE t;
             ALTER PUBLICATION p SET TABLE t, u;
             ALTER PUBLICATION p RENAME TO p2;
             DROP PUBLICATION p2, q;
             DROP PUBLICATION IF EXISTS nosuch;",
        ),
    ]);
}

#[test]
fn event_triggers_are_validated_and_tracked() {
    // PG 18 CreateEventTrigger / AlterEventTrigger / dependency on the
    // function.
    let setup = "CREATE FUNCTION ef() RETURNS event_trigger LANGUAGE plpgsql AS 'begin end';
                 CREATE FUNCTION nf() RETURNS int LANGUAGE sql AS 'select 1';
                 CREATE EVENT TRIGGER e ON ddl_command_start EXECUTE FUNCTION ef();";
    for (stmt, msg) in [
        (
            "CREATE EVENT TRIGGER x ON ddl_command_start EXECUTE FUNCTION nosuch();",
            "function nosuch() does not exist",
        ),
        (
            "CREATE EVENT TRIGGER x ON nosuch_event EXECUTE FUNCTION ef();",
            "unrecognized event name \"nosuch_event\"",
        ),
        (
            "CREATE EVENT TRIGGER x ON ddl_command_start EXECUTE FUNCTION nf();",
            "function nf must return type event_trigger",
        ),
        (
            "CREATE EVENT TRIGGER e ON ddl_command_end EXECUTE FUNCTION ef();",
            "event trigger \"e\" already exists",
        ),
        (
            "ALTER EVENT TRIGGER nosuch DISABLE;",
            "event trigger \"nosuch\" does not exist",
        ),
        (
            "DROP EVENT TRIGGER nosuch;",
            "event trigger \"nosuch\" does not exist",
        ),
        (
            "DROP FUNCTION ef();",
            "cannot drop function ef() because other objects depend on it",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER EVENT TRIGGER e DISABLE;
             ALTER EVENT TRIGGER e RENAME TO e2;
             DROP EVENT TRIGGER IF EXISTS nosuch;
             CREATE EVENT TRIGGER e3 ON sql_drop EXECUTE FUNCTION ef();
             DROP EVENT TRIGGER e2;
             DROP FUNCTION ef() CASCADE;",
        ),
    ]);
}

#[test]
fn event_trigger_tag_filters_are_validated() {
    // PG 18 CreateEventTrigger: validate_ddl_tags / validate_table_rewrite_tags
    // against cmdtaglist.h.
    let setup = "CREATE FUNCTION ef() RETURNS event_trigger LANGUAGE plpgsql AS 'begin end';";
    for (stmt, msg) in [
        (
            "CREATE EVENT TRIGGER et ON ddl_command_start WHEN tag IN ('CREATE TABLE', 'bogus')
             EXECUTE FUNCTION ef();",
            "filter value \"bogus\" not recognized for filter variable \"tag\"",
        ),
        (
            "CREATE EVENT TRIGGER et ON ddl_command_end WHEN tag IN ('CREATE EVENT TRIGGER')
             EXECUTE FUNCTION ef();",
            "event triggers are not supported for CREATE EVENT TRIGGER",
        ),
        (
            "CREATE EVENT TRIGGER et ON sql_drop WHEN tag IN ('create database')
             EXECUTE FUNCTION ef();",
            "event triggers are not supported for create database",
        ),
        (
            "CREATE EVENT TRIGGER et ON ddl_command_start WHEN tag IN ('CREATE ROLE')
             EXECUTE FUNCTION ef();",
            "event triggers are not supported for CREATE ROLE",
        ),
        (
            "CREATE EVENT TRIGGER et ON table_rewrite WHEN tag IN ('CREATE TABLE')
             EXECUTE FUNCTION ef();",
            "event triggers are not supported for CREATE TABLE",
        ),
        (
            "CREATE EVENT TRIGGER et ON table_rewrite WHEN tag IN ('ALTER DOMAIN')
             EXECUTE FUNCTION ef();",
            "event triggers are not supported for ALTER DOMAIN",
        ),
        (
            "CREATE EVENT TRIGGER et ON table_rewrite WHEN tag IN ('bogus')
             EXECUTE FUNCTION ef();",
            "event triggers are not supported for bogus",
        ),
        (
            "CREATE EVENT TRIGGER et ON ddl_command_start WHEN bogus IN ('CREATE TABLE')
             EXECUTE FUNCTION ef();",
            "unrecognized filter variable \"bogus\"",
        ),
        (
            "CREATE EVENT TRIGGER et ON ddl_command_start
             WHEN tag IN ('CREATE TABLE') AND tag IN ('DROP TABLE') EXECUTE FUNCTION ef();",
            "filter variable \"tag\" specified more than once",
        ),
        (
            "CREATE EVENT TRIGGER et ON login WHEN tag IN ('LOGIN') EXECUTE FUNCTION ef();",
            "tag filtering is not supported for login event triggers",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE EVENT TRIGGER e1 ON ddl_command_start
                 WHEN tag IN ('create table', 'DROP TABLE', 'COMMENT') EXECUTE FUNCTION ef();
             CREATE EVENT TRIGGER e2 ON table_rewrite
                 WHEN tag IN ('ALTER TABLE', 'ALTER TYPE', 'alter materialized view')
                 EXECUTE FUNCTION ef();
             CREATE EVENT TRIGGER e3 ON sql_drop WHEN tag IN ('DROP INDEX') EXECUTE FUNCTION ef();",
        ),
    ]);
}

#[test]
fn unlogged_tables_follow_persistence_rules() {
    // PG 18 ATAddForeignKeyConstraint / ATPrepChangePersistence /
    // transformCreateStmt / DefineView.
    let setup = "CREATE TABLE p (id int PRIMARY KEY);
                 CREATE UNLOGGED TABLE u (id int PRIMARY KEY);
                 CREATE UNLOGGED TABLE f3 (x int REFERENCES u);
                 CREATE TABLE f4 (x int REFERENCES p);";
    for (stmt, msg) in [
        (
            "CREATE TABLE f1 (x int REFERENCES u);",
            "constraints on permanent tables may reference only permanent tables",
        ),
        (
            "CREATE TABLE f1 (x int); ALTER TABLE f1 ADD FOREIGN KEY (x) REFERENCES u;",
            "constraints on permanent tables may reference only permanent tables",
        ),
        (
            "ALTER TABLE f3 SET LOGGED;",
            "could not change table \"f3\" to logged because it references unlogged table \"u\"",
        ),
        (
            "ALTER TABLE p SET UNLOGGED;",
            "could not change table \"p\" to unlogged because it references logged table \"f4\"",
        ),
        (
            "CREATE UNLOGGED TABLE pu (a int) PARTITION BY LIST (a);",
            "partitioned tables cannot be unlogged",
        ),
        (
            "CREATE UNLOGGED VIEW v AS SELECT 1;",
            "views cannot be unlogged because they do not have storage",
        ),
        (
            "CREATE UNLOGGED TABLE c AS SELECT 1 AS id; CREATE TABLE f5 (x int REFERENCES u);",
            "constraints on permanent tables may reference only permanent tables",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE UNLOGGED TABLE f2 (x int REFERENCES p);
             ALTER TABLE u SET LOGGED;
             ALTER TABLE f3 SET LOGGED;
             CREATE UNLOGGED SEQUENCE s;
             ALTER TABLE f4 SET UNLOGGED;
             ALTER TABLE p SET UNLOGGED;",
        ),
    ]);
}

#[test]
fn temporary_relations_belong_to_the_migration_session() {
    // PG 18: temporary relations live in the session's pg_temp schema,
    // searched first; ON COMMIT needs a temporary table; a view reading a
    // temporary relation is temporary; foreign keys don't cross
    // persistence.
    let setup = "CREATE TABLE p (id int PRIMARY KEY);
                 CREATE TEMP TABLE t1 (a int);";
    for (stmt, msg) in [
        (
            "CREATE TEMP TABLE public.t2 (a int);",
            "cannot create temporary relation in non-temporary schema",
        ),
        (
            "CREATE TABLE t4 (a int) ON COMMIT DROP;",
            "ON COMMIT can only be used on temporary tables",
        ),
        (
            "CREATE TEMP TABLE t5 (a int REFERENCES p);",
            "constraints on temporary tables may reference only temporary tables",
        ),
        (
            "CREATE TABLE t6 (a int REFERENCES t1);",
            "constraints on permanent tables may reference only permanent tables",
        ),
        (
            "CREATE TEMP TABLE p (x int); SELECT id FROM p;",
            "column \"id\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE pg_temp.t3 (a int);
             INSERT INTO t1 VALUES (1);
             INSERT INTO pg_temp.t3 SELECT a FROM t1;
             CREATE VIEW v1 AS SELECT * FROM t1;
             CREATE TEMP SEQUENCE ts;
             CREATE INDEX ON t1 (a);
             CREATE TEMP VIEW tv AS SELECT 1 AS x;
             CREATE TEMP TABLE t8 (a int) ON COMMIT DELETE ROWS;
             CREATE TEMP TABLE p (x int);
             SELECT x FROM p;",
        ),
        ("0003.sql", "SELECT a FROM t1; SELECT * FROM v1;"),
    ]);
    // ON COMMIT DROP: gone once the migration's transaction commits.
    let err = try_apply(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TEMP TABLE t7 (a int) ON COMMIT DROP; INSERT INTO t7 VALUES (1);",
        ),
        ("0003.sql", "INSERT INTO t7 VALUES (1);"),
    ])
    .expect_err("ON COMMIT DROP");
    assert!(
        err.to_string()
            .starts_with("relation \"t7\" does not exist"),
        "got: {err}"
    );
    // The application's sessions don't see the migrations' temporary
    // relations.
    for name in ["t1", "t3", "v1", "ts", "tv", "t8"] {
        assert!(db.resolve_table(None, name).is_none(), "{name} is visible");
    }
    let p = db.resolve_table(None, "p").expect("p");
    assert_eq!(db.namespace_name(p.relnamespace), Some("public"));
}

#[test]
fn text_search_objects_are_tracked() {
    // PG 18 tsearchcmds.c and the regconfig / regdictionary /
    // regnamespace / regcollation input functions.
    let setup = "CREATE TABLE t (a text);
                 CREATE TEXT SEARCH CONFIGURATION c3 (COPY = english);";
    for (stmt, msg) in [
        (
            "CREATE TEXT SEARCH CONFIGURATION c1 (COPY = nosuch);",
            "text search configuration \"nosuch\" does not exist",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION c2 (PARSER = nosuch);",
            "text search parser \"nosuch\" does not exist",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION c3 (COPY = english);",
            "duplicate key value violates unique constraint \"pg_ts_config_cfgname_index\"",
        ),
        (
            "ALTER TEXT SEARCH CONFIGURATION nosuch ADD MAPPING FOR word WITH simple;",
            "text search configuration \"nosuch\" does not exist",
        ),
        (
            "ALTER TEXT SEARCH CONFIGURATION c3 ALTER MAPPING FOR word WITH nosuch;",
            "text search dictionary \"nosuch\" does not exist",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d1 (TEMPLATE = nosuch);",
            "text search template \"nosuch\" does not exist",
        ),
        (
            "DROP TEXT SEARCH CONFIGURATION nosuch;",
            "text search configuration \"nosuch\" does not exist",
        ),
        (
            "SELECT to_tsvector('nosuch', 'x');",
            "text search configuration \"nosuch\" does not exist",
        ),
        (
            "SELECT 'nosuch'::regdictionary;",
            "text search dictionary \"nosuch\" does not exist",
        ),
        (
            "SELECT 'nosuch'::regnamespace;",
            "schema \"nosuch\" does not exist",
        ),
        (
            "SELECT 'nosuch'::regcollation;",
            "collation \"nosuch\" for encoding \"UTF8\" does not exist",
        ),
        (
            "DROP TEXT SEARCH CONFIGURATION c3; SELECT to_tsvector('c3', 'x');",
            "text search configuration \"c3\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = simple);
             ALTER TEXT SEARCH CONFIGURATION c3 ALTER MAPPING FOR word WITH d2, simple;
             ALTER TEXT SEARCH DICTIONARY d2 (STOPWORDS = english);
             SELECT to_tsvector('c3', 'x'), 'english'::regconfig, 'simple'::regdictionary,
                    'public'::regnamespace;
             COMMENT ON TEXT SEARCH CONFIGURATION c3 IS 'x';
             DROP TEXT SEARCH CONFIGURATION c3;
             DROP TEXT SEARCH DICTIONARY IF EXISTS nosuch;",
        ),
    ]);
}

#[test]
fn text_search_renames_and_moves_are_seen_by_regconfig() {
    // PG 18 AlterObjectRename_internal / AlterObjectNamespace_internal on
    // text search objects, and the regconfig / regdictionary lookups.
    let setup = "CREATE SCHEMA s;
                 CREATE TEXT SEARCH CONFIGURATION myc (COPY = english);
                 CREATE TEXT SEARCH CONFIGURATION other (COPY = english);
                 CREATE TEXT SEARCH DICTIONARY myd (TEMPLATE = simple);
                 ALTER TEXT SEARCH CONFIGURATION myc RENAME TO myc2;";
    for (stmt, msg) in [
        (
            "SELECT to_tsvector('myc', 'x');",
            "text search configuration \"myc\" does not exist",
        ),
        (
            "ALTER TEXT SEARCH CONFIGURATION myc2 RENAME TO other;",
            "text search configuration \"other\" already exists in schema \"public\"",
        ),
        (
            "ALTER TEXT SEARCH CONFIGURATION nosuch RENAME TO x;",
            "text search configuration \"nosuch\" does not exist",
        ),
        (
            "ALTER TEXT SEARCH CONFIGURATION myc2 SET SCHEMA s; SELECT 'myc2'::regconfig;",
            "text search configuration \"myc2\" does not exist",
        ),
        (
            "ALTER TEXT SEARCH DICTIONARY myd RENAME TO myd2; SELECT 'myd'::regdictionary;",
            "text search dictionary \"myd\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "SELECT to_tsvector('myc2', 'x'), 'myc2'::regconfig;
             ALTER TEXT SEARCH CONFIGURATION myc2 SET SCHEMA s;
             ALTER TEXT SEARCH DICTIONARY myd RENAME TO myd2;
             SELECT 's.myc2'::regconfig, 'myd2'::regdictionary;",
        ),
    ]);
    db.analyze("SELECT to_tsvector('s.myc2', 'x') AS x")
        .unwrap();
}

#[test]
fn text_search_options_are_validated() {
    // PG 18 DefineTSConfiguration / DefineTSDictionary / getTokenTypes /
    // verify_dictoptions with the built-in templates' init methods
    // (dsimple_init, dsnowball_init, dispell_init, dsynonym_init,
    // thesaurus_init) and the stock tsearch_data files.
    let setup = "CREATE TEXT SEARCH CONFIGURATION myc (PARSER = default);
                 CREATE TEXT SEARCH DICTIONARY d (TEMPLATE = simple);";
    for (stmt, msg) in [
        (
            "ALTER TEXT SEARCH CONFIGURATION myc ADD MAPPING FOR bogus WITH simple;",
            "token type \"bogus\" does not exist",
        ),
        (
            "ALTER TEXT SEARCH CONFIGURATION english DROP MAPPING FOR asciiword, bogus;",
            "token type \"bogus\" does not exist",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION c2 (PARSER = default, COPY = english);",
            "cannot specify both PARSER and COPY options",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION c2 (BOGUS = x);",
            "text search configuration parameter \"bogus\" not recognized",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (BOGUS = x);",
            "text search template is required",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = simple, BOGUS = x);",
            "unrecognized simple dictionary parameter: \"bogus\"",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = simple, STOPWORDS = nosuchfile);",
            "could not open stop-word file \"/usr/share/postgresql/18/tsearch_data/nosuchfile.stop\"",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = simple, STOPWORDS = 'English');",
            "invalid text search configuration file name \"English\"",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = simple, STOPWORDS = english, STOPWORDS = english);",
            "multiple StopWords parameters",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = simple, ACCEPT = maybe);",
            "accept requires a Boolean value",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = snowball);",
            "missing Language parameter",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = snowball, LANGUAGE = klingon);",
            "no Snowball stemmer available for language \"klingon\" and encoding \"UTF8\"",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = ispell, DICTFILE = ispell_sample);",
            "missing AffFile parameter",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = ispell, DICTFILE = nosuch, AFFFILE = ispell_sample);",
            "could not open dictionary file \"/usr/share/postgresql/18/tsearch_data/nosuch.dict\"",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = synonym);",
            "missing Synonyms parameter",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = synonym, SYNONYMS = nosuch);",
            "could not open synonym file \"/usr/share/postgresql/18/tsearch_data/nosuch.syn\"",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = thesaurus, DICTFILE = thesaurus_sample);",
            "missing Dictionary parameter",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = thesaurus, DICTFILE = thesaurus_sample,
             DICTIONARY = nosuch);",
            "text search dictionary \"nosuch\" does not exist",
        ),
        (
            "ALTER TEXT SEARCH DICTIONARY d (BOGUS = 1);",
            "unrecognized simple dictionary parameter: \"bogus\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TEXT SEARCH CONFIGURATION myc ADD MAPPING FOR asciiword, word WITH simple;
             CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = simple, STOPWORDS = english, ACCEPT = false);
             CREATE TEXT SEARCH DICTIONARY d3 (TEMPLATE = snowball, LANGUAGE = 'French');
             CREATE TEXT SEARCH DICTIONARY d4 (TEMPLATE = ispell, DICTFILE = ispell_sample,
                 AFFFILE = ispell_sample, STOPWORDS = english);
             CREATE TEXT SEARCH DICTIONARY d5 (TEMPLATE = synonym, SYNONYMS = synonym_sample);
             ALTER TEXT SEARCH DICTIONARY d2 (STOPWORDS);
             ALTER TEXT SEARCH DICTIONARY d (STOPWORDS = german);",
        ),
    ]);
}

#[test]
fn languages_must_exist() {
    // PG 18 get_language_oid / DropProceduralLanguage / CreateTransform.
    let setup = "CREATE TABLE t (a text);";
    for (stmt, msg) in [
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE nosuch AS 'x';",
            "language \"nosuch\" does not exist",
        ),
        (
            "DROP LANGUAGE nosuch;",
            "language \"nosuch\" does not exist",
        ),
        (
            "DROP LANGUAGE plpgsql;",
            "cannot drop language plpgsql because extension plpgsql requires it",
        ),
        (
            "DROP LANGUAGE sql;",
            "cannot drop language sql because it is required by the database system",
        ),
        (
            "ALTER LANGUAGE nosuch RENAME TO x;",
            "language \"nosuch\" does not exist",
        ),
        (
            "CREATE TRANSFORM FOR int LANGUAGE nosuch (FROM SQL WITH FUNCTION f(internal));",
            "language \"nosuch\" does not exist",
        ),
        (
            "DO $$ BEGIN END $$ LANGUAGE nosuch;",
            "language \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'select 1';
             CREATE FUNCTION g() RETURNS int LANGUAGE plpgsql AS 'begin return 1; end';
             DROP LANGUAGE IF EXISTS nosuch;
             CREATE OR REPLACE LANGUAGE plpgsql;",
        ),
    ]);
}

#[test]
fn plpgsql_return_statements_follow_the_function_result() {
    // PG 18 make_return_stmt (pl_gram.y).
    let setup = "CREATE TABLE t (a int);";
    for (stmt, msg) in [
        (
            "CREATE PROCEDURE pr() LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;",
            "RETURN cannot have a parameter in a procedure",
        ),
        (
            "CREATE FUNCTION fv() RETURNS void LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;",
            "RETURN cannot have a parameter in function returning void",
        ),
        (
            "CREATE FUNCTION fi() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN; END $$;",
            "missing expression at or near \";\"",
        ),
        (
            "CREATE FUNCTION fs() RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;",
            "RETURN cannot have a parameter in function returning set",
        ),
        (
            "CREATE FUNCTION fo(OUT a int) LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;",
            "RETURN cannot have a parameter in function with OUT parameters",
        ),
        (
            "CREATE FUNCTION ft() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN; END $$;",
            "missing expression at or near \";\"",
        ),
        (
            "CREATE FUNCTION fe() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;",
            "RETURN cannot have a parameter in function returning void",
        ),
        (
            "DO $$ BEGIN RETURN 1; END $$;",
            "RETURN cannot have a parameter in function returning void",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE FUNCTION a1() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
             CREATE FUNCTION a2() RETURNS void LANGUAGE plpgsql AS $$ BEGIN RETURN; END $$;
             CREATE FUNCTION a3() RETURNS SETOF int LANGUAGE plpgsql AS $$
               BEGIN RETURN NEXT 1; RETURN QUERY SELECT 2; RETURN; END $$;
             CREATE FUNCTION a4(INOUT a int) LANGUAGE plpgsql AS $$ BEGIN RETURN; END $$;
             CREATE PROCEDURE a5(INOUT a int) LANGUAGE plpgsql AS $$ BEGIN RETURN; END $$;
             CREATE FUNCTION a6() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
             DO $$ BEGIN RETURN; END $$;",
        ),
    ]);
}

#[test]
fn identity_sequence_options_are_validated() {
    // PG 18: an identity column's sequence options go through init_params
    // (CREATE TABLE, ADD GENERATED, ALTER COLUMN SET / RESTART), and
    // SEQUENCE NAME names the sequence.
    let setup = "CREATE TABLE c (id int GENERATED ALWAYS AS IDENTITY);";
    for (stmt, msg) in [
        (
            "CREATE TABLE a (id int GENERATED ALWAYS AS IDENTITY (START WITH 10 MAXVALUE 5));",
            "START value (10) cannot be greater than MAXVALUE (5)",
        ),
        (
            "CREATE TABLE b (id smallint GENERATED BY DEFAULT AS IDENTITY (MAXVALUE 100000));",
            "MAXVALUE (100000) is out of range for sequence data type smallint",
        ),
        (
            "ALTER TABLE c ALTER COLUMN id SET MAXVALUE 0;",
            "MINVALUE (1) must be less than MAXVALUE (0)",
        ),
        (
            "ALTER TABLE c ALTER COLUMN id RESTART WITH 0;",
            "RESTART value (0) cannot be less than MINVALUE (1)",
        ),
        (
            "ALTER TABLE c ALTER COLUMN id SET INCREMENT BY 0;",
            "INCREMENT must not be zero",
        ),
        (
            "CREATE TABLE d (x int NOT NULL); ALTER TABLE d ALTER COLUMN x ADD GENERATED ALWAYS AS IDENTITY (INCREMENT 0);",
            "INCREMENT must not be zero",
        ),
        (
            "ALTER TABLE c ADD COLUMN y bigint GENERATED ALWAYS AS IDENTITY (CACHE 0);",
            "CACHE (0) must be greater than zero",
        ),
        (
            "CREATE TABLE e (id int GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME eseq)); ALTER SEQUENCE e_id_seq RESTART;",
            "relation \"e_id_seq\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE e (id int GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME eseq START 5));
             SELECT last_value FROM eseq;
             ALTER TABLE c ALTER COLUMN id SET MAXVALUE 1000;
             ALTER TABLE c ALTER COLUMN id RESTART WITH 10;
             ALTER TABLE c ALTER COLUMN id SET GENERATED BY DEFAULT SET INCREMENT BY 2;",
        ),
    ]);
}

#[test]
fn functions_follow_pseudo_type_rules() {
    // PG 18 ProcedureCreate / fmgr_sql_validator / plpgsql_validator.
    let setup = "CREATE TABLE t (a text);";
    for (stmt, msg) in [
        (
            "CREATE FUNCTION f1(cstring) RETURNS int LANGUAGE sql AS 'select 1';",
            "SQL functions cannot have arguments of type cstring",
        ),
        (
            "CREATE FUNCTION f2(internal) RETURNS int LANGUAGE sql AS 'select 1';",
            "SQL functions cannot have arguments of type internal",
        ),
        (
            "CREATE FUNCTION f4() RETURNS cstring LANGUAGE sql AS 'select 1';",
            "SQL functions cannot return type cstring",
        ),
        (
            "CREATE FUNCTION f5() RETURNS internal LANGUAGE sql AS 'select 1';",
            "unsafe use of pseudo-type \"internal\"",
        ),
        (
            "CREATE FUNCTION f6() RETURNS trigger LANGUAGE sql AS 'select 1';",
            "SQL functions cannot return type trigger",
        ),
        (
            "CREATE FUNCTION f8(trigger) RETURNS int LANGUAGE sql AS 'select 1';",
            "SQL functions cannot have arguments of type trigger",
        ),
        (
            "CREATE FUNCTION f10(cstring) RETURNS int LANGUAGE plpgsql AS 'begin return 1; end';",
            "PL/pgSQL functions cannot accept type cstring",
        ),
        (
            "CREATE FUNCTION f11() RETURNS cstring LANGUAGE plpgsql AS 'begin return 1; end';",
            "PL/pgSQL functions cannot return type cstring",
        ),
        (
            "CREATE FUNCTION f12() RETURNS internal LANGUAGE c AS 'x', 'y';",
            "unsafe use of pseudo-type \"internal\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE FUNCTION f3(anyelement) RETURNS int LANGUAGE sql AS 'select 1';
             CREATE FUNCTION f9() RETURNS trigger LANGUAGE plpgsql AS 'begin return null; end';
             CREATE FUNCTION f13() RETURNS void LANGUAGE sql AS 'select';
             CREATE FUNCTION f14() RETURNS record LANGUAGE sql AS 'select 1, 2';
             CREATE FUNCTION f15(record) RETURNS int LANGUAGE plpgsql AS 'begin return 1; end';
             CREATE PROCEDURE p1() LANGUAGE sql AS 'select 1';",
        ),
    ]);
}

#[test]
fn procedure_and_function_result_types() {
    // PG 18 CreateFunction: a function without RETURNS needs OUT
    // parameters; a procedure returns void, or record with OUT parameters.
    let err = try_apply(&[(
        "0001.sql",
        "CREATE FUNCTION f() LANGUAGE sql AS 'select 1';",
    )])
    .expect_err("no result type");
    assert!(
        err.to_string()
            .starts_with("function result type must be specified"),
        "got: {err}"
    );
    let db = build_db(&[(
        "0001.sql",
        "CREATE PROCEDURE p0() LANGUAGE sql AS 'select 1';
         CREATE PROCEDURE p1(OUT a int) LANGUAGE sql AS 'select 1';
         CREATE PROCEDURE p2(OUT a int, OUT b int) LANGUAGE sql AS 'select 1, 2';
         CREATE FUNCTION f1(OUT a int) LANGUAGE sql AS 'select 1';",
    )]);
    let seed = db.to_seed();
    let rettype = |name: &str| {
        let proc = seed.pg_proc.iter().find(|p| p.proname == name).unwrap();
        seed.pg_type
            .iter()
            .find(|t| t.oid == proc.prorettype)
            .map(|t| t.typname.clone())
            .unwrap()
    };
    assert_eq!(rettype("p0"), "void");
    assert_eq!(rettype("p1"), "record");
    assert_eq!(rettype("p2"), "record");
    assert_eq!(rettype("f1"), "int4");
}

#[test]
fn conversions_are_validated_and_tracked() {
    // PG 18 CreateConversionCommand / get_conversion_oid.
    let setup = "CREATE CONVERSION cv FOR 'LATIN1' TO 'UTF8' FROM iso8859_1_to_utf8;";
    for (stmt, msg) in [
        (
            "CREATE CONVERSION cv FOR 'LATIN1' TO 'UTF8' FROM iso8859_1_to_utf8;",
            "conversion \"cv\" already exists",
        ),
        (
            "CREATE CONVERSION cv2 FOR 'LATIN1' TO 'UTF8' FROM nosuch;",
            "function nosuch(integer, integer, cstring, internal, integer, boolean) does not exist",
        ),
        (
            "DROP CONVERSION nosuch;",
            "conversion \"nosuch\" does not exist",
        ),
        (
            "ALTER CONVERSION nosuch RENAME TO x;",
            "conversion \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER CONVERSION cv RENAME TO cv2;
             DROP CONVERSION cv2;
             DROP CONVERSION IF EXISTS nosuch;",
        ),
    ]);
}

#[test]
fn deferrable_constraint_indexes_are_non_immediate() {
    // PG 18 ATExecReplicaIdentity: a DEFERRABLE constraint's index has
    // indimmediate = false — including the partition clones, LIKE copies
    // and indexes adopted by ADD CONSTRAINT ... USING INDEX ... DEFERRABLE.
    let setup = "CREATE TABLE t (a int NOT NULL, CONSTRAINT u UNIQUE (a) DEFERRABLE INITIALLY IMMEDIATE);
                 CREATE TABLE c (a int NOT NULL UNIQUE DEFERRABLE);
                 CREATE TABLE p (a int NOT NULL, CONSTRAINT pu UNIQUE (a) DEFERRABLE) PARTITION BY LIST (a);
                 CREATE TABLE p1 PARTITION OF p FOR VALUES IN (1);
                 CREATE TABLE l (LIKE t INCLUDING INDEXES);
                 CREATE TABLE s (a int NOT NULL);
                 CREATE UNIQUE INDEX ui ON s (a);
                 ALTER TABLE s ADD CONSTRAINT uc UNIQUE USING INDEX ui DEFERRABLE;
                 CREATE TABLE k (a int NOT NULL);
                 ALTER TABLE k ADD CONSTRAINT kp PRIMARY KEY (a) DEFERRABLE;";
    for (stmt, index) in [
        ("ALTER TABLE t REPLICA IDENTITY USING INDEX u;", "u"),
        (
            "ALTER TABLE c REPLICA IDENTITY USING INDEX c_a_key;",
            "c_a_key",
        ),
        (
            "ALTER TABLE p1 REPLICA IDENTITY USING INDEX p1_a_key;",
            "p1_a_key",
        ),
        (
            "ALTER TABLE l REPLICA IDENTITY USING INDEX l_a_key;",
            "l_a_key",
        ),
        ("ALTER TABLE s REPLICA IDENTITY USING INDEX uc;", "uc"),
        ("ALTER TABLE k REPLICA IDENTITY USING INDEX kp;", "kp"),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        let msg = format!("cannot use non-immediate index \"{index}\" as replica identity");
        assert!(err.to_string().starts_with(&msg), "{stmt}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int NOT NULL, CONSTRAINT u UNIQUE (a) NOT DEFERRABLE);
         ALTER TABLE t REPLICA IDENTITY USING INDEX u;",
    )]);
}

#[test]
fn publication_row_filters_are_validated() {
    // PG 18 TransformPubWhereClauses: the filter is a WHERE clause
    // (EXPR_KIND_WHERE, coerced to boolean) limited by
    // check_simple_rowfilter_expr to columns, constants, built-in operators,
    // types and collations, and immutable built-in functions.
    let setup = "CREATE TYPE e AS ENUM ('x');
                 CREATE TABLE t (a int, b text, c e, d int[], j jsonb);
                 CREATE FUNCTION f(int) RETURNS bool IMMUTABLE LANGUAGE sql AS 'select true';";
    let invalid = "invalid publication WHERE expression";
    for (filter, msg) in [
        (
            "a + 1",
            "argument of PUBLICATION WHERE must be type boolean, not type integer",
        ),
        ("nosuch > 1", "column \"nosuch\" does not exist"),
        (
            "count(*) > 1",
            "aggregate functions are not allowed in WHERE",
        ),
        (
            "row_number() over () > 1",
            "window functions are not allowed in WHERE",
        ),
        (
            "generate_series(1, 2) > 1",
            "set-returning functions are not allowed in WHERE",
        ),
        ("random() > 0.5", invalid),
        ("now() > '2020-01-01'", invalid),
        ("current_date > '2020-01-01'", invalid),
        ("f(a)", invalid),
        ("a > (select 1)", invalid),
        ("a::text = '1'", invalid),
        ("b::int = 1", invalid),
        ("c = 'x'", invalid),
        ("d[1] = 1", invalid),
        ("ctid > '(0,1)'", invalid),
    ] {
        let stmt = format!("CREATE PUBLICATION p FOR TABLE t WHERE ({filter});");
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", &stmt)]).expect_err(&stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
        let stmt =
            format!("CREATE PUBLICATION p; ALTER PUBLICATION p ADD TABLE t WHERE ({filter});");
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", &stmt)]).expect_err(&stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE PUBLICATION p1 FOR TABLE t WHERE (a::bigint = 1);
             CREATE PUBLICATION p2 FOR TABLE t WHERE (b LIKE 'x%' AND a IN (1, 2)
                 AND a IS NOT NULL AND coalesce(a, 1) = 1 AND j->>'k' = 'v');
             CREATE PUBLICATION p3 FOR TABLE t WHERE ('t');
             CREATE PUBLICATION p4 FOR TABLE t WHERE (b = 'x' COLLATE \"C\");
             CREATE PUBLICATION p5 FOR TABLE t WHERE (CASE WHEN a > 1 THEN true
                 ELSE a BETWEEN 1 AND 3 END AND greatest(a, 2) > 1
                 AND a IS DISTINCT FROM 3 AND nullif(a, 1) > 0 AND a = ANY (ARRAY[1, 2])
                 AND ROW(a, b) = ROW(1, 'x') AND length(b) > 1 AND b::varchar = 'x');",
        ),
    ]);
}

#[test]
fn publications_take_only_publishable_tables_and_schemas() {
    // PG 18 check_publication_add_relation / check_publication_add_schema
    // and CheckAlterPublication.
    let setup = "CREATE TABLE t (a int PRIMARY KEY, b text);
                 CREATE UNLOGGED TABLE tu (a int);
                 CREATE PUBLICATION pall FOR ALL TABLES;
                 CREATE PUBLICATION p;";
    for (stmt, msg) in [
        (
            "ALTER PUBLICATION pall ADD TABLE t;",
            "publication \"pall\" is defined as FOR ALL TABLES",
        ),
        (
            "ALTER PUBLICATION pall DROP TABLE t;",
            "publication \"pall\" is defined as FOR ALL TABLES",
        ),
        (
            "ALTER PUBLICATION pall SET TABLES IN SCHEMA public;",
            "publication \"pall\" is defined as FOR ALL TABLES",
        ),
        (
            "CREATE TEMP TABLE tt (a int); CREATE PUBLICATION q FOR TABLE tt;",
            "cannot add relation \"tt\" to publication",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE tu;",
            "cannot add relation \"tu\" to publication",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE pg_catalog.pg_class;",
            "cannot add relation \"pg_class\" to publication",
        ),
        (
            "ALTER PUBLICATION p ADD TABLE pg_catalog.pg_class;",
            "cannot add relation \"pg_class\" to publication",
        ),
        (
            "CREATE PUBLICATION q FOR TABLES IN SCHEMA pg_catalog;",
            "cannot add schema \"pg_catalog\" to publication",
        ),
        (
            "ALTER PUBLICATION p ADD TABLES IN SCHEMA pg_toast;",
            "cannot add schema \"pg_toast\" to publication",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER PUBLICATION pall SET (publish = 'insert');
             ALTER PUBLICATION p ADD TABLE t;
             ALTER PUBLICATION p ADD TABLES IN SCHEMA public;",
        ),
    ]);
}

#[test]
fn publication_membership_rules() {
    // PG 18 parse_publication_options / OpenTableList /
    // TransformPubWhereClauses / CheckPubRelationColumnList /
    // pub_collist_validate / PublicationDropTables / AlterPublicationSchemas
    // / AlterPublicationOptions.
    let setup = "CREATE TABLE t (a int, b int);
                 CREATE TABLE pt (a int, b int) PARTITION BY LIST (a);
                 CREATE VIEW v AS SELECT 1 AS a;
                 CREATE PUBLICATION p FOR TABLE t;
                 CREATE PUBLICATION r FOR TABLE pt WHERE (a > 1) WITH (publish_via_partition_root);
                 CREATE PUBLICATION rc FOR TABLE pt (a) WITH (publish_via_partition_root);
                 CREATE PUBLICATION h FOR TABLE t (a);";
    for (stmt, msg) in [
        (
            "ALTER PUBLICATION p DROP TABLE t WHERE (a > 1);",
            "cannot use a WHERE clause when removing a table from a publication",
        ),
        (
            "ALTER PUBLICATION p DROP TABLE t (a);",
            "column list must not be specified in ALTER PUBLICATION ... DROP",
        ),
        (
            "ALTER PUBLICATION p DROP TABLE v;",
            "relation \"v\" is not part of the publication",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE t WHERE (a > 1), t;",
            "conflicting or redundant WHERE clauses for table \"t\"",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE t (a), t;",
            "conflicting or redundant column lists for table \"t\"",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE pt WHERE (a > 1);",
            "cannot use publication WHERE clause for relation \"pt\"",
        ),
        (
            "CREATE PUBLICATION q; ALTER PUBLICATION q ADD TABLE pt WHERE (a > 1);",
            "cannot use publication WHERE clause for relation \"pt\"",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE pt (a);",
            "cannot use column list for relation \"public.pt\" in publication \"q\"",
        ),
        (
            "CREATE PUBLICATION q FOR TABLES IN SCHEMA public, TABLE t (a);",
            "cannot use column list for relation \"public.t\" in publication \"q\"",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE t (a, a);",
            "duplicate column \"a\" in publication column list",
        ),
        (
            "CREATE PUBLICATION q FOR TABLE t (ctid);",
            "cannot use system column \"ctid\" in publication column list",
        ),
        (
            "ALTER PUBLICATION h ADD TABLES IN SCHEMA public;",
            "cannot add schema to publication \"h\"",
        ),
        (
            "ALTER PUBLICATION r SET (publish_via_partition_root = false);",
            "cannot set parameter \"publish_via_partition_root\" to false for publication \"r\"",
        ),
        (
            "ALTER PUBLICATION rc SET (publish_via_partition_root = off);",
            "cannot set parameter \"publish_via_partition_root\" to false for publication \"rc\"",
        ),
        (
            "CREATE PUBLICATION q WITH (publish_via_partition_root = 'maybe');",
            "publish_via_partition_root requires a Boolean value",
        ),
        (
            "CREATE PUBLICATION q WITH (publish_via_partition_root = 'yes');",
            "publish_via_partition_root requires a Boolean value",
        ),
        (
            "CREATE PUBLICATION q WITH (publish_generated_columns = 'maybe');",
            "invalid value for publication parameter \"publish_generated_columns\": \"maybe\"",
        ),
        (
            "CREATE PUBLICATION q WITH (publish = 'insert', publish = 'update');",
            "conflicting or redundant options",
        ),
        (
            "CREATE PUBLICATION q WITH (publish);",
            "publish requires a parameter",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE PUBLICATION q1 FOR TABLE t, t;
             CREATE PUBLICATION q2 WITH (publish = 'INSERT', publish_generated_columns = 'STORED',
                 publish_via_partition_root = 1);
             CREATE PUBLICATION q3 WITH (publish_via_partition_root = 'ON');
             ALTER PUBLICATION q3 ADD TABLE pt WHERE (a > 1), t, t;
             ALTER PUBLICATION q3 SET TABLE t WHERE (a > 1), pt WHERE (b > 1);
             ALTER PUBLICATION r SET (publish_via_partition_root = true);
             ALTER PUBLICATION p SET (publish_via_partition_root = false);",
        ),
    ]);
}

#[test]
fn index_columns_are_named_for_set_statistics() {
    // PG 18 ATExecSetStatistics over the index's own attributes, named by
    // ChooseIndexColumnNames (the column name, or "expr", made unique with
    // a numeric suffix).
    let setup = "CREATE TABLE t (a int, b int);
                 CREATE INDEX i ON t (a, (b + 1), (a * 2), a);";
    for (stmt, msg) in [
        (
            "ALTER INDEX i ALTER COLUMN a SET STATISTICS 100;",
            "cannot alter statistics on non-expression column \"a\" of index \"i\"",
        ),
        (
            "ALTER INDEX i ALTER COLUMN a1 SET STATISTICS 100;",
            "cannot alter statistics on non-expression column \"a1\" of index \"i\"",
        ),
        (
            "ALTER INDEX i ALTER COLUMN 4 SET STATISTICS 100;",
            "cannot alter statistics on non-expression column \"a1\" of index \"i\"",
        ),
        (
            "ALTER INDEX i ALTER COLUMN b SET STATISTICS 100;",
            "column \"b\" of relation \"i\" does not exist",
        ),
        (
            "ALTER TABLE t ALTER COLUMN 1 SET STATISTICS 100;",
            "cannot refer to non-index column by number",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER INDEX i ALTER COLUMN expr SET STATISTICS 100;
             ALTER INDEX i ALTER COLUMN expr1 SET STATISTICS 100;
             ALTER INDEX i ALTER COLUMN 3 SET STATISTICS 100;",
        ),
    ]);
}

#[test]
fn index_include_columns_are_modeled() {
    // PG 18 ComputeIndexAttrs / transformIndexConstraint: INCLUDE columns
    // are non-key index columns — part of the generated index name, of the
    // index's dependencies, but not of the constraint key.
    let setup = "CREATE TABLE t (a int, b int);
                 CREATE INDEX ON t (a) INCLUDE (b);
                 CREATE UNIQUE INDEX u ON t (a) INCLUDE (b);
                 CREATE TABLE s (a int, b int, UNIQUE (a) INCLUDE (b),
                     EXCLUDE USING btree (b WITH =) INCLUDE (a));
                 CREATE TABLE k (a int, b int, CONSTRAINT kp PRIMARY KEY (a) INCLUDE (b));
                 CREATE TABLE k2 (a int, b int, PRIMARY KEY (a) INCLUDE (b));";
    for (stmt, msg) in [
        (
            "CREATE INDEX x ON t (a) INCLUDE ((b + 1));",
            "expressions are not supported in included columns",
        ),
        (
            "CREATE TABLE s2 (a int, UNIQUE (a) INCLUDE (nosuch));",
            "column \"nosuch\" named in key does not exist",
        ),
        (
            "ALTER TABLE t ADD UNIQUE (a) INCLUDE (nosuch);",
            "column \"nosuch\" named in key does not exist",
        ),
        (
            "ALTER INDEX u ALTER COLUMN 2 SET STATISTICS 100;",
            "cannot alter statistics on included column \"b\" of index \"u\"",
        ),
        (
            "ALTER INDEX kp ALTER COLUMN b SET STATISTICS 100;",
            "cannot alter statistics on included column \"b\" of index \"kp\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "DROP INDEX t_a_b_idx;
             ALTER INDEX s_a_b_key RENAME TO s1;
             ALTER INDEX s_b_a_excl RENAME TO s2;
             ALTER TABLE t ADD UNIQUE (b) INCLUDE (a);
             ALTER INDEX t_b_a_key RENAME TO t1;
             CREATE TABLE l (LIKE s INCLUDING INDEXES);
             ALTER INDEX l_a_b_key RENAME TO l1;
             ALTER TABLE k DROP COLUMN b;
             ALTER TABLE k ADD CONSTRAINT kp PRIMARY KEY (a);",
        ),
    ]);
    // The PRIMARY KEY's INCLUDE column isn't made NOT NULL.
    let seed = db.to_seed();
    let k = seed
        .pg_class
        .iter()
        .find(|c| c.relname == "k2")
        .unwrap()
        .oid;
    let b = seed
        .pg_attribute
        .iter()
        .find(|a| a.attrelid == k && a.attname == "b")
        .unwrap();
    assert!(!b.attnotnull);
}

#[test]
fn a_table_has_at_most_one_primary_key() {
    // PG 18 index_check_primary_key, reached by every PRIMARY KEY added
    // after the table exists (or copied by LIKE INCLUDING INDEXES).
    let setup = "CREATE TABLE k (a int PRIMARY KEY, b int NOT NULL);
                 CREATE UNIQUE INDEX kb ON k (b);
                 CREATE TABLE m (a int, b int);";
    for (stmt, table) in [
        ("ALTER TABLE k ADD PRIMARY KEY (b);", "k"),
        ("ALTER TABLE k ADD PRIMARY KEY USING INDEX kb;", "k"),
        (
            "ALTER TABLE m ADD PRIMARY KEY (a), ADD PRIMARY KEY (b);",
            "m",
        ),
        (
            "CREATE TABLE l (LIKE k INCLUDING INDEXES, PRIMARY KEY (b));",
            "l",
        ),
        (
            "CREATE TABLE l2 (LIKE k INCLUDING INDEXES); ALTER TABLE l2 ADD PRIMARY KEY (b);",
            "l2",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        let msg = format!("multiple primary keys for table \"{table}\" are not allowed");
        assert!(err.to_string().starts_with(&msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE m ADD PRIMARY KEY (a);
             ALTER TABLE k DROP CONSTRAINT k_pkey;
             ALTER TABLE k ADD PRIMARY KEY USING INDEX kb;",
        ),
    ]);
}

#[test]
fn like_including_identity_copies_the_sequence_options() {
    // PG 18 transformTableLikeClause: the new identity sequence gets the
    // source sequence's options (sequence_options).
    let setup = "CREATE TABLE s (id int GENERATED ALWAYS AS IDENTITY
                     (MINVALUE 10 START 20 INCREMENT 5 MAXVALUE 1000 CACHE 3), x int);
                 CREATE TABLE b (id bigint GENERATED BY DEFAULT AS IDENTITY
                     (MINVALUE 5000000000 MAXVALUE 9000000000));
                 CREATE TABLE l (LIKE s INCLUDING IDENTITY);
                 CREATE TABLE lb (LIKE b INCLUDING IDENTITY);";
    for (stmt, msg) in [
        (
            "ALTER TABLE l ALTER COLUMN id RESTART WITH 5;",
            "RESTART value (5) cannot be less than MINVALUE (10)",
        ),
        (
            "ALTER TABLE l ALTER COLUMN id RESTART WITH 1001;",
            "RESTART value (1001) cannot be greater than MAXVALUE (1000)",
        ),
        (
            "ALTER TABLE lb ALTER COLUMN id RESTART WITH 1;",
            "RESTART value (1) cannot be less than MINVALUE (5000000000)",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        ("0002.sql", "ALTER TABLE l ALTER COLUMN id RESTART WITH 10;"),
    ]);
}

#[test]
fn conversion_encodings_must_exist() {
    // PG 18 CreateConversionCommand: pg_char_to_encoding over the cleaned
    // name (alphanumerics, lowercased) and its alias table; SQL_ASCII can't
    // be converted.
    for (stmt, msg) in [
        (
            "CREATE CONVERSION c FOR 'nosuch' TO 'UTF8' FROM iso8859_1_to_utf8;",
            "source encoding \"nosuch\" does not exist",
        ),
        (
            "CREATE CONVERSION c FOR 'LATIN1' TO 'utf16' FROM iso8859_1_to_utf8;",
            "destination encoding \"utf16\" does not exist",
        ),
        (
            "CREATE CONVERSION c FOR 'SQL_ASCII' TO 'UTF8' FROM iso8859_1_to_utf8;",
            "encoding conversion to or from \"SQL_ASCII\" is not supported",
        ),
        (
            "CREATE CONVERSION c FOR 'LATIN1' TO 'sql-ascii' FROM iso8859_1_to_utf8;",
            "encoding conversion to or from \"SQL_ASCII\" is not supported",
        ),
    ] {
        let err = try_apply(&[("0001.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE CONVERSION c1 FOR 'latin-1' TO 'utf-8' FROM iso8859_1_to_utf8;
         CREATE CONVERSION c2 FOR 'ISO_8859_1' TO 'unicode' FROM iso8859_1_to_utf8;
         CREATE CONVERSION c3 FOR 'Windows1252' TO 'UTF8' FROM win_to_utf8;",
    )]);
}

#[test]
fn rule_actions_are_analyzed_over_new_and_old() {
    // PG 18 transformRuleStmt: each action is analyzed with NEW / OLD as
    // relation-only range entries (qualified references only).
    let setup = "CREATE TYPE pair AS (x int, y int);
                 CREATE TABLE t (a int, name text, p pair);
                 CREATE TABLE log (id int, msg text);";
    for (stmt, msg) in [
        (
            "CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO log VALUES (new.nosuch);",
            "column new.nosuch does not exist",
        ),
        (
            "CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO log (id) VALUES (new.name);",
            "column \"id\" is of type integer but expression is of type text",
        ),
        (
            "CREATE RULE r AS ON UPDATE TO t DO ALSO UPDATE log SET msg = new.name WHERE nosuch = 1;",
            "column \"nosuch\" does not exist",
        ),
        (
            "CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO log VALUES (new.a, a);",
            "column \"a\" does not exist",
        ),
        (
            "CREATE RULE r AS ON DELETE TO t DO ALSO INSERT INTO log VALUES (old.a || 'x', 'y');",
            "column \"id\" is of type integer but expression is of type text",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE RULE r4 AS ON UPDATE TO t DO ALSO (
                 INSERT INTO log VALUES (old.a, new.name);
                 DELETE FROM log WHERE log.id = old.a;
                 NOTIFY ch;
                 SELECT (new.p).x, new.a + 1
             );
             CREATE RULE r7 AS ON INSERT TO t DO ALSO INSERT INTO log VALUES (new.a, new.ctid::text);
             CREATE RULE r8 AS ON INSERT TO t DO INSTEAD NOTHING;",
        ),
    ]);
}

#[test]
fn rule_actions_see_whole_row_and_star_new_old() {
    // PG 18: `new.*`, a whole-row `new` / `old` and references from a
    // subquery are analyzed too, not skipped.
    let setup = "CREATE TABLE src (id int PRIMARY KEY, name text NOT NULL, amount int);
                 CREATE TABLE log1 (id int, name text, amount int);
                 CREATE TABLE log2 (r src);
                 CREATE TABLE log3 (id int, info text);";
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE RULE r1 AS ON INSERT TO src DO ALSO INSERT INTO log1 SELECT new.*;
             CREATE RULE r2 AS ON INSERT TO src DO ALSO INSERT INTO log2 VALUES (new);
             CREATE RULE r3 AS ON UPDATE TO src DO ALSO INSERT INTO log3 SELECT old.id, (SELECT new.name);
             CREATE RULE r8 AS ON DELETE TO src DO ALSO INSERT INTO log2 SELECT old;",
        ),
    ]);
    for (stmt, msg) in [
        (
            "CREATE RULE r6 AS ON INSERT TO src DO ALSO INSERT INTO log3 VALUES (new.id, new.amount + 'a'::text);",
            "operator does not exist: integer + text",
        ),
        (
            "CREATE RULE r7 AS ON INSERT TO src DO ALSO INSERT INTO log3 SELECT new.*;",
            "INSERT has more expressions than target columns",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
}

#[test]
fn publication_row_filters_judge_node_kind_then_type() {
    // PG 18's check_simple_rowfilter_expr_walker: the node kind first (a
    // cast to a domain is a disallowed CoerceToDomain, a cast through I/O
    // too), then whether its type is user-defined; a cast literal or
    // `ARRAY[…]` constructor leaves no coercion node behind.
    let setup = "CREATE TYPE mood AS ENUM ('a', 'b');
                 CREATE TYPE pair AS (a int, b text);
                 CREATE DOMAIN d AS int;
                 CREATE TABLE pt (id int PRIMARY KEY, m mood, n int, dd d);";
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE PUBLICATION p FOR TABLE pt WHERE (n > 1);",
        ),
    ]);
    for (filter, detail) in [
        (
            r#"greatest(m, m) = 'a'"#,
            r#"User-defined types are not allowed."#,
        ),
        (r#"m = 'a'"#, r#"User-defined types are not allowed."#),
        (
            r#"coalesce(m, 'b') IS NULL"#,
            r#"User-defined types are not allowed."#,
        ),
        (
            r#"'a'::mood IS NULL"#,
            r#"User-defined types are not allowed."#,
        ),
        (
            r#"(CASE WHEN n > 0 THEN m END) IS NULL"#,
            r#"User-defined types are not allowed."#,
        ),
        (
            r#"(ARRAY[m])[1] IS NULL"#,
            r#"Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed."#,
        ),
        (
            r#"m IN ('a', 'b')"#,
            r#"User-defined types are not allowed."#,
        ),
        (
            r#"ROW(m, n) IS NULL"#,
            r#"User-defined types are not allowed."#,
        ),
        (
            r#"n = 1 OR m IS NULL"#,
            r#"User-defined types are not allowed."#,
        ),
        (
            r#"m::text = 'a'"#,
            r#"Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed."#,
        ),
        (
            r#"n::text = 'a'"#,
            r#"Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed."#,
        ),
        (
            r#"ARRAY['a']::mood[] IS NULL"#,
            r#"User-defined types are not allowed."#,
        ),
        (r#"dd > 1"#, r#"User-defined types are not allowed."#),
        (r#"dd::int > 1"#, r#"User-defined types are not allowed."#),
        (
            r#"(n::d) > 1"#,
            r#"Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed."#,
        ),
        (
            r#"nullif(m, 'a') IS NULL"#,
            r#"User-defined types are not allowed."#,
        ),
        (
            r#"ARRAY[m] IS NULL"#,
            r#"User-defined types are not allowed."#,
        ),
        (
            r#"(ROW(1, 'x')::pair).a = 1"#,
            r#"Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed."#,
        ),
        (
            r#"1::d > 1"#,
            r#"Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed."#,
        ),
        (
            r#"(n::d)::int > 1"#,
            r#"Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed."#,
        ),
        (
            r#"'a'::mood::text = 'a'"#,
            r#"Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed."#,
        ),
    ] {
        let stmt = format!("CREATE PUBLICATION p FOR TABLE pt WHERE ({});", filter);
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", &stmt)]).expect_err(filter);
        assert!(
            err.to_string()
                .starts_with(&format!("invalid publication WHERE expression ({detail})")),
            "{filter}\n  got: {err}"
        );
    }
}

#[test]
fn conversion_function_must_handle_the_encoding_pair() {
    // PG 18 runs the conversion function once; the built-in C functions
    // reject pairs they don't implement. Messages as PG raises them.
    for (stmt, msg) in [
        (
            "CREATE CONVERSION c FOR 'LATIN1' TO 'UTF8' FROM utf8_to_iso8859_1;",
            "expected source encoding \"UTF8\", but got \"LATIN1\"",
        ),
        (
            "CREATE CONVERSION c FOR 'KOI8R' TO 'EUC_JP' FROM koi8r_to_mic;",
            "expected destination encoding \"MULE_INTERNAL\", but got \"EUC_JP\"",
        ),
        (
            "CREATE CONVERSION c FOR 'UTF8' TO 'EUC_JP' FROM utf8_to_iso8859;",
            "unexpected encoding ID 1 for ISO 8859 character sets",
        ),
        (
            "CREATE CONVERSION c FOR 'UTF8' TO 'EUC_JP' FROM utf8_to_win;",
            "unexpected encoding ID 1 for WIN character sets",
        ),
    ] {
        let err = try_apply(&[("0001.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE CONVERSION c1 FOR 'LATIN2' TO 'UTF8' FROM iso8859_to_utf8;
         CREATE CONVERSION c2 FOR 'WIN1252' TO 'UTF8' FROM win_to_utf8;",
    )]);
}

#[test]
fn grammar_errors_carry_pg_message_verbatim() {
    // gram.y's own errors (PG 18): the message is PG's, with no prefix.
    for (stmt, msg) in [
        (
            "CREATE TABLE t (a int GENERATED BY DEFAULT AS (1) STORED);",
            "for a generated column, GENERATED ALWAYS must be specified",
        ),
        (
            "CREATE TABLE t (a int, CONSTRAINT x NOT NULL a DEFERRABLE);",
            "NOT NULL constraints cannot be marked DEFERRABLE",
        ),
        ("CREATE TABLE t (a int,);", "syntax error at or near \")\""),
    ] {
        let err = try_apply(&[("0001.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
}

#[test]
fn unsupported_relation_kinds_are_named_like_pg() {
    // errdetail_relkind_not_supported names a partitioned index as such.
    let setup = "CREATE TABLE pt (a int) PARTITION BY RANGE (a);
                 CREATE INDEX pti ON pt (a);";
    for (sql, message) in [
        (
            "ALTER SEQUENCE pti RESTART;",
            "cannot open relation \"pti\" (This operation is not supported for partitioned indexes.)",
        ),
        (
            "SELECT 1; LOCK TABLE pti;",
            "cannot lock relation \"pti\" (This operation is not supported for partitioned indexes.)",
        ),
        (
            "ALTER SEQUENCE pt RESTART;",
            "cannot open relation \"pt\" (This operation is not supported for partitioned tables.)",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", sql)]).expect_err(sql);
        assert_eq!(err.to_string(), message, "{sql}");
    }
    // A partitioned index (relkind I) is still an index to ALTER / DROP
    // INDEX.
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER INDEX pti RENAME TO pti2; DROP INDEX pti2;",
        ),
    ]);
}

#[test]
fn truncate_ignores_not_enforced_foreign_keys() {
    // heap_truncate_find_FKs skips a NOT ENFORCED foreign key; a NOT VALID
    // one still counts.
    let setup = "CREATE TABLE a (x int PRIMARY KEY);
                 CREATE TABLE b (y int REFERENCES a NOT ENFORCED);
                 CREATE TABLE a2 (x int PRIMARY KEY);
                 CREATE TABLE b2 (y int);
                 ALTER TABLE b2 ADD CONSTRAINT fk FOREIGN KEY (y) REFERENCES a2 NOT VALID;";
    for stmt in [
        "TRUNCATE a2;",
        "ALTER TABLE b ALTER CONSTRAINT b_y_fkey ENFORCED; TRUNCATE a;",
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(
            err.to_string()
                .starts_with("cannot truncate a table referenced in a foreign key constraint"),
            "{stmt}\n  got: {err}"
        );
    }
    build_db(&[("0001.sql", setup), ("0002.sql", "TRUNCATE a;")]);
}

#[test]
fn truncate_only_refuses_a_partitioned_table() {
    // ExecuteTruncate: ONLY on a partitioned table would truncate nothing.
    let setup = "CREATE TABLE p (x int) PARTITION BY RANGE (x);
                 CREATE TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);
                 CREATE TABLE q (x int) PARTITION BY LIST (x);";
    for stmt in ["TRUNCATE ONLY p;", "TRUNCATE ONLY q;"] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(
            err.to_string()
                .starts_with("cannot truncate only a partitioned table"),
            "{stmt}\n  got: {err}"
        );
    }
    build_db(&[
        ("0001.sql", setup),
        ("0002.sql", "TRUNCATE p; TRUNCATE ONLY c;"),
    ]);
}

#[test]
fn truncate_refuses_a_system_catalog() {
    // truncate_check_rel: a relation with a pinned OID is a system catalog,
    // off limits even to a superuser without allow_system_table_mods; the
    // information_schema tables aren't.
    for (stmt, msg) in [
        (
            "TRUNCATE pg_class;",
            "permission denied: \"pg_class\" is a system catalog",
        ),
        (
            "TRUNCATE ONLY pg_catalog.pg_largeobject;",
            "permission denied: \"pg_largeobject\" is a system catalog",
        ),
    ] {
        let err = try_apply(&[("0001.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[("0001.sql", "TRUNCATE information_schema.sql_features;")]);
}
