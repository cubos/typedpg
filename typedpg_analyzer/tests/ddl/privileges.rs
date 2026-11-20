//! Roles, ownership and privileges: what the catalog records for CREATE ROLE,
//! GRANT / REVOKE, ALTER DEFAULT PRIVILEGES, ALTER ... OWNER, REASSIGN OWNED
//! and DROP OWNED, and the `has_*_privilege` queries the later privilege
//! checker will use. Under `pg_sanity`, the oracle also compares every
//! created role's privilege matrix with a real PostgreSQL.

use crate::common::*;
use typedpg_analyzer::Privilege;

const MIGRATIONS_USER: &str = PgCatalog::MIGRATIONS_USER;

fn relation(db: &PgCatalog, schema: &str, name: &str) -> PgClassOid {
    class_oid(db, Some(schema), name)
}

fn namespace(db: &PgCatalog, name: &str) -> PgNamespaceOid {
    db.namespace_oid(name)
        .unwrap_or_else(|| panic!("schema {name} not found"))
}

fn proc_oid(db: &PgCatalog, schema: &str, name: &str) -> PgProcOid {
    db.find_functions(Some(schema), name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("function {schema}.{name} not found"))
        .oid
}

fn type_oid(db: &PgCatalog, schema: &str, name: &str) -> PgTypeOid {
    db.resolve_type_by_name(Some(schema), name)
        .unwrap_or_else(|| panic!("type {schema}.{name} not found"))
        .oid
}

fn attnum(db: &PgCatalog, relid: PgClassOid, name: &str) -> i16 {
    db.attributes_of(relid)
        .iter()
        .find(|a| a.attname == name)
        .unwrap_or_else(|| panic!("column {name} not found"))
        .attnum
}

fn table_has(db: &PgCatalog, role: &str, schema: &str, name: &str, privilege: Privilege) -> bool {
    db.has_table_privilege(role, relation(db, schema, name), privilege)
}

#[test]
fn a_new_table_belongs_to_the_migrations_user_only() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_owner_app;
         CREATE TABLE acl_owner_t (id int, name text);",
    )]);
    assert!(db.role_exists("acl_owner_app"));
    assert!(!db.role_exists("acl_owner_external"));
    let table = relation(&db, "public", "acl_owner_t");
    for privilege in [
        Privilege::Select,
        Privilege::Insert,
        Privilege::Update,
        Privilege::Delete,
        Privilege::Truncate,
        Privilege::References,
        Privilege::Trigger,
        Privilege::Maintain,
    ] {
        assert!(
            db.has_table_privilege(MIGRATIONS_USER, table, privilege),
            "the migrating user owns the table: {privilege:?}"
        );
        assert!(
            db.has_table_privilege("postgres", table, privilege),
            "the bootstrap superuser is a superuser: {privilege:?}"
        );
        assert!(
            !db.has_table_privilege("acl_owner_app", table, privilege),
            "a fresh role has no privileges: {privilege:?}"
        );
    }
    assert!(!db.has_any_column_privilege("acl_owner_app", table, Privilege::Select));
}

#[test]
fn objects_created_under_set_role_belong_to_it() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_setrole_owner;
         CREATE SCHEMA acl_setrole_s AUTHORIZATION acl_setrole_owner;
         SET ROLE acl_setrole_owner;
         CREATE TABLE acl_setrole_s.t (id int);
         CREATE SEQUENCE acl_setrole_s.seq;
         CREATE VIEW acl_setrole_s.v AS SELECT id FROM acl_setrole_s.t;
         CREATE FUNCTION acl_setrole_s.f() RETURNS int LANGUAGE sql AS 'select 1';
         CREATE TYPE acl_setrole_s.e AS ENUM ('a');
         RESET ROLE;",
    )]);
    let table = relation(&db, "acl_setrole_s", "t");
    assert!(db.has_table_privilege("acl_setrole_owner", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_setrole_owner", table, Privilege::Insert));
    assert!(db.has_table_privilege("acl_setrole_owner", table, Privilege::Trigger));
    assert!(db.has_table_privilege(
        "acl_setrole_owner",
        relation(&db, "acl_setrole_s", "v"),
        Privilege::Select
    ));
    assert!(db.has_sequence_privilege(
        "acl_setrole_owner",
        relation(&db, "acl_setrole_s", "seq"),
        Privilege::Usage
    ));
    assert!(db.has_function_privilege("acl_setrole_owner", proc_oid(&db, "acl_setrole_s", "f")));
    assert!(db.has_type_privilege("acl_setrole_owner", type_oid(&db, "acl_setrole_s", "e")));
    // The migrations' user is superuser-equivalent, so it still can.
    assert!(db.has_table_privilege(MIGRATIONS_USER, table, Privilege::Delete));
}

#[test]
fn grant_and_revoke_table_privileges() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_grant_app;
         CREATE TABLE acl_grant_t (id int, a text, b text);
         GRANT SELECT, INSERT ON acl_grant_t TO acl_grant_app;",
    )]);
    let table = relation(&db, "public", "acl_grant_t");
    assert!(db.has_table_privilege("acl_grant_app", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_grant_app", table, Privilege::Insert));
    assert!(!db.has_table_privilege("acl_grant_app", table, Privilege::Update));
    assert!(!db.has_table_privilege("acl_grant_app", table, Privilege::Delete));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_grant_all_app;
             CREATE TABLE acl_grant_all_t (id int);
             GRANT ALL PRIVILEGES ON acl_grant_all_t TO acl_grant_all_app;",
        ),
        (
            "0002.sql",
            "REVOKE DELETE, TRUNCATE ON acl_grant_all_t FROM acl_grant_all_app;",
        ),
    ]);
    let table = relation(&db, "public", "acl_grant_all_t");
    for privilege in [
        Privilege::Select,
        Privilege::Insert,
        Privilege::Update,
        Privilege::References,
        Privilege::Trigger,
        Privilege::Maintain,
    ] {
        assert!(
            db.has_table_privilege("acl_grant_all_app", table, privilege),
            "{privilege:?} was granted by ALL"
        );
    }
    assert!(!db.has_table_privilege("acl_grant_all_app", table, Privilege::Delete));
    assert!(!db.has_table_privilege("acl_grant_all_app", table, Privilege::Truncate));
}

#[test]
fn column_privileges_and_the_implicit_table_revoke() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_col_app;
             CREATE TABLE acl_col_t (id int, a text, b text);
             GRANT SELECT (a), UPDATE (b) ON acl_col_t TO acl_col_app;",
    )]);
    let table = relation(&db, "public", "acl_col_t");
    let a = attnum(&db, table, "a");
    let b = attnum(&db, table, "b");
    assert!(db.has_column_privilege("acl_col_app", table, a, Privilege::Select));
    assert!(db.has_column_privilege("acl_col_app", table, b, Privilege::Update));
    assert!(!db.has_column_privilege("acl_col_app", table, b, Privilege::Select));
    assert!(!db.has_table_privilege("acl_col_app", table, Privilege::Select));
    assert!(db.has_any_column_privilege("acl_col_app", table, Privilege::Select));
    assert!(!db.has_any_column_privilege("acl_col_app", table, Privilege::Delete));
    assert!(db.has_any_column_privilege("acl_col_app", table, Privilege::Update));

    // A table-level REVOKE also revokes the same privilege at column level
    // (the SQL standard's rule), while other column privileges stay.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_col_revoke_app;
             CREATE TABLE acl_col_revoke_t (id int, a text, b text);
             GRANT SELECT ON acl_col_revoke_t TO acl_col_revoke_app;
             GRANT SELECT (a) ON acl_col_revoke_t TO acl_col_revoke_app;
             GRANT UPDATE (b) ON acl_col_revoke_t TO acl_col_revoke_app;",
        ),
        (
            "0002.sql",
            "REVOKE SELECT ON acl_col_revoke_t FROM acl_col_revoke_app;",
        ),
    ]);
    let table = relation(&db, "public", "acl_col_revoke_t");
    let a = attnum(&db, table, "a");
    let b = attnum(&db, table, "b");
    assert!(!db.has_column_privilege("acl_col_revoke_app", table, a, Privilege::Select));
    assert!(!db.has_table_privilege("acl_col_revoke_app", table, Privilege::Select));
    assert!(db.has_column_privilege("acl_col_revoke_app", table, b, Privilege::Update));

    // A column added later starts without an ACL.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_col_add_app;
             CREATE TABLE acl_col_add_t (a text);
             GRANT SELECT (a) ON acl_col_add_t TO acl_col_add_app;",
        ),
        ("0002.sql", "ALTER TABLE acl_col_add_t ADD COLUMN b text;"),
    ]);
    let table = relation(&db, "public", "acl_col_add_t");
    let b = attnum(&db, table, "b");
    assert!(!db.has_column_privilege("acl_col_add_app", table, b, Privilege::Select));
    assert!(db.has_any_column_privilege("acl_col_add_app", table, Privilege::Select));
}

#[test]
fn grants_to_public_reach_every_role() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_public_app;
             CREATE TABLE acl_public_t (id int);
             GRANT SELECT ON acl_public_t TO PUBLIC;",
    )]);
    let table = relation(&db, "public", "acl_public_t");
    assert!(db.has_table_privilege("acl_public_app", table, Privilege::Select));
    assert!(db.has_column_privilege(
        "acl_public_app",
        table,
        attnum(&db, table, "id"),
        Privilege::Select
    ));
    assert!(!db.has_table_privilege("acl_public_app", table, Privilege::Insert));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_fn_public_app;
             CREATE FUNCTION acl_fn_public_f() RETURNS int LANGUAGE sql AS 'select 1';",
        ),
        (
            "0002.sql",
            "REVOKE EXECUTE ON FUNCTION acl_fn_public_f() FROM PUBLIC;",
        ),
    ]);
    let function = proc_oid(&db, "public", "acl_fn_public_f");
    assert!(!db.has_function_privilege("acl_fn_public_app", function));
    assert!(db.has_function_privilege(MIGRATIONS_USER, function));

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_fn_grant_app;
         CREATE FUNCTION acl_fn_grant_f() RETURNS int LANGUAGE sql AS 'select 1';
         REVOKE EXECUTE ON FUNCTION acl_fn_grant_f() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION acl_fn_grant_f() TO acl_fn_grant_app;",
    )]);
    assert!(db.has_function_privilege(
        "acl_fn_grant_app",
        proc_oid(&db, "public", "acl_fn_grant_f")
    ));
}

#[test]
fn overloads_with_named_and_out_arguments_have_separate_acls() {
    // Each overload has its own ACL; argument names and OUT parameters
    // aren't part of its identity.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_fn_overload_app;
         CREATE FUNCTION acl_fn_overload(x integer, OUT y text) LANGUAGE sql
             AS 'select x::text';
         CREATE FUNCTION acl_fn_overload(label character varying, n bigint DEFAULT 1)
             RETURNS int LANGUAGE sql AS 'select 1';
         REVOKE EXECUTE ON FUNCTION acl_fn_overload(integer) FROM PUBLIC;",
    )]);
    let overloads = db.find_functions(Some("public"), "acl_fn_overload");
    assert_eq!(overloads.len(), 2);
    for f in overloads {
        let executable = db.has_function_privilege("acl_fn_overload_app", f.oid);
        assert_eq!(executable, f.proargtypes.len() == 2, "{:?}", f.proargtypes);
    }
}

#[test]
fn schema_privileges_and_the_public_schema() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_schema_app;
             CREATE SCHEMA acl_schema_s;
             GRANT USAGE ON SCHEMA acl_schema_s TO acl_schema_app;",
        ),
        (
            "0002.sql",
            "GRANT CREATE ON SCHEMA acl_schema_s TO acl_schema_app;",
        ),
    ]);
    let schema = namespace(&db, "acl_schema_s");
    assert!(db.has_schema_privilege("acl_schema_app", schema, Privilege::Usage));
    assert!(db.has_schema_privilege("acl_schema_app", schema, Privilege::Create));
    assert!(db.has_schema_privilege(MIGRATIONS_USER, schema, Privilege::Create));
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_schema_public_app;
         REVOKE CREATE ON SCHEMA public FROM PUBLIC;",
    )]);
    let public = namespace(&db, "public");
    assert!(db.has_schema_privilege("acl_schema_public_app", public, Privilege::Usage));
    assert!(!db.has_schema_privilege("acl_schema_public_app", public, Privilege::Create));
}

#[test]
fn sequence_privileges_and_serial_sequences() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_seq_app;
         CREATE SEQUENCE acl_seq_s;
         GRANT USAGE ON SEQUENCE acl_seq_s TO acl_seq_app;
         CREATE TABLE acl_seq_t (id serial, other int);",
    )]);
    let sequence = relation(&db, "public", "acl_seq_s");
    assert!(db.has_sequence_privilege("acl_seq_app", sequence, Privilege::Usage));
    assert!(!db.has_sequence_privilege("acl_seq_app", sequence, Privilege::Select));
    assert!(!db.has_sequence_privilege("acl_seq_app", sequence, Privilege::Update));

    let serial = relation(&db, "public", "acl_seq_t_id_seq");
    assert!(!db.has_sequence_privilege("acl_seq_app", serial, Privilege::Usage));

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_seq_table_app;
             CREATE SEQUENCE acl_seq_table_s;
             GRANT USAGE, SELECT ON TABLE acl_seq_table_s TO acl_seq_table_app;",
    )]);
    let sequence = relation(&db, "public", "acl_seq_table_s");
    assert!(db.has_sequence_privilege("acl_seq_table_app", sequence, Privilege::Usage));
    assert!(db.has_sequence_privilege("acl_seq_table_app", sequence, Privilege::Select));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_seq_serial_app;
             CREATE TABLE acl_seq_serial_t (id serial primary key);",
        ),
        (
            "0002.sql",
            "GRANT USAGE ON SEQUENCE acl_seq_serial_t_id_seq TO acl_seq_serial_app;",
        ),
    ]);
    assert!(db.has_sequence_privilege(
        "acl_seq_serial_app",
        relation(&db, "public", "acl_seq_serial_t_id_seq"),
        Privilege::Usage
    ));
    assert!(!db.has_table_privilege(
        "acl_seq_serial_app",
        relation(&db, "public", "acl_seq_serial_t"),
        Privilege::Usage
    ));
}

#[test]
fn type_privileges_are_usage_and_arrays_follow_their_element() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_type_app;
             CREATE TYPE acl_type_e AS ENUM ('a', 'b');
             CREATE DOMAIN acl_type_d AS int;",
        ),
        (
            "0002.sql",
            "REVOKE USAGE ON TYPE acl_type_e FROM PUBLIC;
             GRANT USAGE ON DOMAIN acl_type_d TO acl_type_app;",
        ),
    ]);
    let enum_ty = type_oid(&db, "public", "acl_type_e");
    let domain = type_oid(&db, "public", "acl_type_d");
    assert!(!db.has_type_privilege("acl_type_app", enum_ty));
    assert!(db.has_type_privilege("acl_type_app", domain));
    // The array type follows the element type's ACL.
    let array = type_oid(&db, "public", "_acl_type_e");
    assert!(!db.has_type_privilege("acl_type_app", array));
    assert!(db.has_type_privilege("acl_type_app", type_oid(&db, "public", "_acl_type_d")));
}

#[test]
fn all_tables_in_schema_covers_only_existing_relations() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_all_app;
             CREATE TABLE acl_all_t1 (id int);
             CREATE VIEW acl_all_v AS SELECT id FROM acl_all_t1;",
        ),
        (
            "0002.sql",
            "GRANT SELECT ON ALL TABLES IN SCHEMA public TO acl_all_app;",
        ),
        ("0003.sql", "CREATE TABLE acl_all_t2 (id int);"),
    ]);
    assert!(table_has(
        &db,
        "acl_all_app",
        "public",
        "acl_all_t1",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        "acl_all_app",
        "public",
        "acl_all_v",
        Privilege::Select
    ));
    assert!(!table_has(
        &db,
        "acl_all_app",
        "public",
        "acl_all_t2",
        Privilege::Select
    ));

    // ALL FUNCTIONS excludes procedures; ALL PROCEDURES only has them; ALL
    // ROUTINES has both. Revoking PUBLIC's EXECUTE first makes the grants
    // visible.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_all_fn_app;
         CREATE FUNCTION acl_all_fn_f() RETURNS int LANGUAGE sql AS 'select 1';
         CREATE PROCEDURE acl_all_fn_p() LANGUAGE sql AS 'select 1';
         REVOKE EXECUTE ON FUNCTION acl_all_fn_f() FROM PUBLIC;
         REVOKE EXECUTE ON PROCEDURE acl_all_fn_p() FROM PUBLIC;
         GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO acl_all_fn_app;",
    )]);
    assert!(db.has_function_privilege("acl_all_fn_app", proc_oid(&db, "public", "acl_all_fn_f")));
    assert!(!db.has_function_privilege("acl_all_fn_app", proc_oid(&db, "public", "acl_all_fn_p")));

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_all_proc_app;
         CREATE FUNCTION acl_all_proc_f() RETURNS int LANGUAGE sql AS 'select 1';
         CREATE PROCEDURE acl_all_proc_p() LANGUAGE sql AS 'select 1';
         REVOKE EXECUTE ON FUNCTION acl_all_proc_f() FROM PUBLIC;
         REVOKE EXECUTE ON PROCEDURE acl_all_proc_p() FROM PUBLIC;
         GRANT EXECUTE ON ALL PROCEDURES IN SCHEMA public TO acl_all_proc_app;",
    )]);
    assert!(!db.has_function_privilege(
        "acl_all_proc_app",
        proc_oid(&db, "public", "acl_all_proc_f")
    ));
    assert!(db.has_function_privilege(
        "acl_all_proc_app",
        proc_oid(&db, "public", "acl_all_proc_p")
    ));
}

#[test]
fn default_privileges_apply_at_creation_only() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_dp_app;
             CREATE TABLE acl_dp_before (id int);
             ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO acl_dp_app;",
        ),
        ("0002.sql", "CREATE TABLE acl_dp_after (id int);"),
    ]);
    assert!(!table_has(
        &db,
        "acl_dp_app",
        "public",
        "acl_dp_before",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        "acl_dp_app",
        "public",
        "acl_dp_after",
        Privilege::Select
    ));

    // IN SCHEMA: only tables created in that schema.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_dp_in_schema_app;
             CREATE SCHEMA acl_dp_s;
             ALTER DEFAULT PRIVILEGES IN SCHEMA acl_dp_s GRANT INSERT ON TABLES TO acl_dp_in_schema_app;",
        ),
        (
            "0002.sql",
            "CREATE TABLE acl_dp_s.t (id int);
             CREATE TABLE acl_dp_in_public (id int);",
        ),
    ]);
    assert!(table_has(
        &db,
        "acl_dp_in_schema_app",
        "acl_dp_s",
        "t",
        Privilege::Insert
    ));
    assert!(!table_has(
        &db,
        "acl_dp_in_schema_app",
        "acl_dp_s",
        "t",
        Privilege::Select
    ));
    assert!(!table_has(
        &db,
        "acl_dp_in_schema_app",
        "public",
        "acl_dp_in_public",
        Privilege::Insert
    ));

    // FOR ROLE: the default privileges follow that role's creations.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_dp_owner;
             CREATE ROLE acl_dp_for_app;
             CREATE SCHEMA acl_dp_s2 AUTHORIZATION acl_dp_owner;
             ALTER DEFAULT PRIVILEGES FOR ROLE acl_dp_owner
                 GRANT SELECT ON TABLES TO acl_dp_for_app;
             SET ROLE acl_dp_owner;
             CREATE TABLE acl_dp_s2.by_owner (id int);
             RESET ROLE;
             CREATE TABLE acl_dp_s2.by_migrations_user (id int);",
    )]);
    assert!(table_has(
        &db,
        "acl_dp_for_app",
        "acl_dp_s2",
        "by_owner",
        Privilege::Select
    ));
    assert!(!table_has(
        &db,
        "acl_dp_for_app",
        "acl_dp_s2",
        "by_migrations_user",
        Privilege::Select
    ));

    // Default privileges on functions can revoke PUBLIC's EXECUTE.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_dp_fn_app;
             ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;",
        ),
        (
            "0002.sql",
            "CREATE FUNCTION acl_dp_fn_f() RETURNS int LANGUAGE sql AS 'select 1';",
        ),
    ]);
    assert!(!db.has_function_privilege("acl_dp_fn_app", proc_oid(&db, "public", "acl_dp_fn_f")));
    assert!(db.has_function_privilege(MIGRATIONS_USER, proc_oid(&db, "public", "acl_dp_fn_f")));
}

#[test]
fn role_membership_inherits_through_inheritable_grants() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_mem_granted;
             CREATE ROLE acl_mem_member;
             CREATE TABLE acl_mem_t (id int);
             GRANT SELECT ON acl_mem_t TO acl_mem_granted;
             GRANT acl_mem_granted TO acl_mem_member;",
    )]);
    assert!(table_has(
        &db,
        "acl_mem_member",
        "public",
        "acl_mem_t",
        Privilege::Select
    ));

    // A NOINHERIT member gets the grant with INHERIT FALSE by default.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_mem_granted2;
             CREATE ROLE acl_mem_noinherit NOINHERIT;
             CREATE TABLE acl_mem_t2 (id int);
             GRANT SELECT ON acl_mem_t2 TO acl_mem_granted2;
             GRANT acl_mem_granted2 TO acl_mem_noinherit;",
    )]);
    assert!(!table_has(
        &db,
        "acl_mem_noinherit",
        "public",
        "acl_mem_t2",
        Privilege::Select
    ));
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_mem_granted3;
             CREATE ROLE acl_mem_noinherit2 NOINHERIT;
             CREATE TABLE acl_mem_t3 (id int);
             GRANT SELECT ON acl_mem_t3 TO acl_mem_granted3;
             GRANT acl_mem_granted3 TO acl_mem_noinherit2;",
        ),
        (
            "0002.sql",
            "GRANT acl_mem_granted3 TO acl_mem_noinherit2 WITH INHERIT TRUE;",
        ),
    ]);
    assert!(table_has(
        &db,
        "acl_mem_noinherit2",
        "public",
        "acl_mem_t3",
        Privilege::Select
    ));

    // WITH INHERIT FALSE, and transitive chains.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_mem_g4;
             CREATE ROLE acl_mem_explicit;
             CREATE ROLE acl_mem_chain1;
             CREATE ROLE acl_mem_chain2;
             CREATE TABLE acl_mem_t4 (id int);
             GRANT SELECT ON acl_mem_t4 TO acl_mem_g4;
             GRANT acl_mem_g4 TO acl_mem_explicit WITH INHERIT FALSE;
             GRANT acl_mem_g4 TO acl_mem_chain1;
             GRANT acl_mem_chain1 TO acl_mem_chain2;",
    )]);
    assert!(!table_has(
        &db,
        "acl_mem_explicit",
        "public",
        "acl_mem_t4",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        "acl_mem_chain2",
        "public",
        "acl_mem_t4",
        Privilege::Select
    ));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_mem_revoked;
             CREATE ROLE acl_mem_after;
             CREATE TABLE acl_mem_t5 (id int);
             GRANT SELECT ON acl_mem_t5 TO acl_mem_revoked;
             GRANT acl_mem_revoked TO acl_mem_after;",
        ),
        ("0002.sql", "REVOKE acl_mem_revoked FROM acl_mem_after;"),
    ]);
    assert!(!table_has(
        &db,
        "acl_mem_after",
        "public",
        "acl_mem_t5",
        Privilege::Select
    ));
}

#[test]
fn superusers_and_predefined_roles_bypass_or_grant() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_su_super SUPERUSER;
         CREATE ROLE acl_pre_read;
         CREATE ROLE acl_pre_write;
         CREATE TABLE acl_pre_t (id int);
         GRANT pg_read_all_data TO acl_pre_read;
         GRANT pg_write_all_data TO acl_pre_write;",
    )]);
    let table = relation(&db, "public", "acl_pre_t");
    for privilege in [Privilege::Select, Privilege::Insert, Privilege::Trigger] {
        assert!(db.has_table_privilege("acl_su_super", table, privilege));
    }
    assert!(db.has_table_privilege("acl_pre_read", table, Privilege::Select));
    assert!(!db.has_table_privilege("acl_pre_read", table, Privilege::Insert));
    assert!(!db.has_table_privilege("acl_pre_write", table, Privilege::Select));
    for privilege in [Privilege::Insert, Privilege::Update, Privilege::Delete] {
        assert!(db.has_table_privilege("acl_pre_write", table, privilege));
    }
    let schema = namespace(&db, "public");
    assert!(db.has_schema_privilege("acl_pre_read", schema, Privilege::Usage));
    assert!(db.has_schema_privilege("acl_pre_write", schema, Privilege::Usage));
    assert!(db.role_exists("pg_read_all_data"));
    assert!(!db.role_exists("acl_pre_never_created"));
}

#[test]
fn grant_options_cascade_through_regrants() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_go_a;
             CREATE ROLE acl_go_b;
             CREATE TABLE acl_go_t (id int);
             GRANT SELECT ON acl_go_t TO acl_go_a WITH GRANT OPTION;
             SET ROLE acl_go_a;
             GRANT SELECT ON acl_go_t TO acl_go_b;
             RESET ROLE;",
    )]);
    let table = relation(&db, "public", "acl_go_t");
    assert!(db.has_table_privilege("acl_go_a", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_go_b", table, Privilege::Select));

    // The migration user's REVOKE acts as the owner: it only sees the grant
    // it made, not the one acl_go_a made.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_go_a2;
             CREATE ROLE acl_go_b2;
             CREATE TABLE acl_go_t2 (id int);
             GRANT SELECT ON acl_go_t2 TO acl_go_a2 WITH GRANT OPTION;
             SET ROLE acl_go_a2;
             GRANT SELECT ON acl_go_t2 TO acl_go_b2;
             RESET ROLE;
             REVOKE SELECT ON acl_go_t2 FROM acl_go_b2;",
    )]);
    assert!(db.has_table_privilege(
        "acl_go_b2",
        relation(&db, "public", "acl_go_t2"),
        Privilege::Select
    ));

    // Revoking the grant option while a regrant exists needs CASCADE.
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_go_a3;
             CREATE ROLE acl_go_b3;
             CREATE TABLE acl_go_t3 (id int);
             GRANT SELECT ON acl_go_t3 TO acl_go_a3 WITH GRANT OPTION;
             SET ROLE acl_go_a3;
             GRANT SELECT ON acl_go_t3 TO acl_go_b3;
             RESET ROLE;
             REVOKE GRANT OPTION FOR SELECT ON acl_go_t3 FROM acl_go_a3;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "dependent privileges exist"
    );
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_go_a4;
             CREATE ROLE acl_go_b4;
             CREATE TABLE acl_go_t4 (id int);
             GRANT SELECT ON acl_go_t4 TO acl_go_a4 WITH GRANT OPTION;
             SET ROLE acl_go_a4;
             GRANT SELECT ON acl_go_t4 TO acl_go_b4;
             RESET ROLE;
             REVOKE GRANT OPTION FOR SELECT ON acl_go_t4 FROM acl_go_a4 CASCADE;",
    )]);
    let table = relation(&db, "public", "acl_go_t4");
    assert!(db.has_table_privilege("acl_go_a4", table, Privilege::Select));
    assert!(!db.has_table_privilege(
        "acl_go_b4",
        relation(&db, "public", "acl_go_t4"),
        Privilege::Select
    ));
}

#[test]
fn owner_changes_transfer_privileges() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_owner_old;
             CREATE ROLE acl_owner_new;
             CREATE SCHEMA acl_owner_s AUTHORIZATION acl_owner_old;
             SET ROLE acl_owner_old;
             CREATE TABLE acl_owner_s.t (id int);
             CREATE SEQUENCE acl_owner_s.seq;
             CREATE TYPE acl_owner_s.e AS ENUM ('a');
             CREATE FUNCTION acl_owner_s.f() RETURNS int LANGUAGE sql AS 'select 1';
             RESET ROLE;
             REVOKE USAGE ON TYPE acl_owner_s.e FROM PUBLIC;
             REVOKE EXECUTE ON FUNCTION acl_owner_s.f() FROM PUBLIC;
             ALTER TABLE acl_owner_s.t OWNER TO acl_owner_new;
             ALTER SEQUENCE acl_owner_s.seq OWNER TO acl_owner_new;
             ALTER TYPE acl_owner_s.e OWNER TO acl_owner_new;
             ALTER FUNCTION acl_owner_s.f() OWNER TO acl_owner_new;",
    )]);
    let table = relation(&db, "acl_owner_s", "t");
    assert!(!db.has_table_privilege("acl_owner_old", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_owner_new", table, Privilege::Select));
    let sequence = relation(&db, "acl_owner_s", "seq");
    assert!(!db.has_sequence_privilege("acl_owner_old", sequence, Privilege::Usage));
    assert!(db.has_sequence_privilege("acl_owner_new", sequence, Privilege::Usage));
    let enum_ty = type_oid(&db, "acl_owner_s", "e");
    assert!(!db.has_type_privilege("acl_owner_old", enum_ty));
    assert!(db.has_type_privilege("acl_owner_new", enum_ty));
    let function = proc_oid(&db, "acl_owner_s", "f");
    assert!(!db.has_function_privilege("acl_owner_old", function));
    assert!(db.has_function_privilege("acl_owner_new", function));

    // A table's indexes, serial sequence and row type follow it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_owner_tbl_old;
             CREATE ROLE acl_owner_tbl_new;
             CREATE SCHEMA acl_owner_tbl_s AUTHORIZATION acl_owner_tbl_old;
             SET ROLE acl_owner_tbl_old;
             CREATE TABLE acl_owner_tbl_s.t (id serial primary key);
             RESET ROLE;
             ALTER TABLE acl_owner_tbl_s.t OWNER TO acl_owner_tbl_new;",
    )]);
    assert!(db.has_sequence_privilege(
        "acl_owner_tbl_new",
        relation(&db, "acl_owner_tbl_s", "t_id_seq"),
        Privilege::Usage
    ));
    assert!(!db.has_sequence_privilege(
        "acl_owner_tbl_old",
        relation(&db, "acl_owner_tbl_s", "t_id_seq"),
        Privilege::Usage
    ));
}

#[test]
fn reassign_owned_moves_objects_and_their_acl_mentions() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_reassign_old;
             CREATE ROLE acl_reassign_new;
             CREATE ROLE acl_reassign_app;
             CREATE SCHEMA acl_reassign_s AUTHORIZATION acl_reassign_old;
             SET ROLE acl_reassign_old;
             CREATE TABLE acl_reassign_s.t (id int);
             GRANT SELECT ON acl_reassign_s.t TO acl_reassign_app;
             RESET ROLE;",
        ),
        (
            "0002.sql",
            "REASSIGN OWNED BY acl_reassign_old TO acl_reassign_new;",
        ),
    ]);
    let table = relation(&db, "acl_reassign_s", "t");
    assert!(!db.has_table_privilege("acl_reassign_old", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_reassign_new", table, Privilege::Select));
    // The grant to the app role survives (its grantor is rewritten).
    assert!(db.has_table_privilege("acl_reassign_app", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_reassign_new", table, Privilege::Insert));
}

#[test]
fn drop_owned_drops_and_revokes_before_drop_role() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_drop_owner;
             CREATE ROLE acl_drop_app;
             CREATE SCHEMA acl_drop_s AUTHORIZATION acl_drop_owner;
             SET ROLE acl_drop_owner;
             CREATE TABLE acl_drop_s.t (id int);
             RESET ROLE;
             CREATE TABLE acl_drop_other (id int);
             GRANT SELECT ON acl_drop_other TO acl_drop_owner WITH GRANT OPTION;
             SET ROLE acl_drop_owner;
             GRANT SELECT ON acl_drop_other TO acl_drop_app;
             RESET ROLE;",
        ),
        (
            "0002.sql",
            "DROP OWNED BY acl_drop_owner;
             DROP ROLE acl_drop_owner;",
        ),
    ]);
    assert!(!db.role_exists("acl_drop_owner"));
    // DROP OWNED revoked the grant the role made too.
    assert!(!db.has_table_privilege(
        "acl_drop_app",
        relation(&db, "public", "acl_drop_other"),
        Privilege::Select
    ));
}

#[test]
fn drop_role_refuses_a_role_that_owns_or_grants() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_droprole_owner;
         CREATE SCHEMA acl_droprole_s AUTHORIZATION acl_droprole_owner;
         SET ROLE acl_droprole_owner;
         CREATE TABLE acl_droprole_s.t (id int);
         RESET ROLE;
         DROP ROLE acl_droprole_owner;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "role \"acl_droprole_owner\" cannot be dropped because some objects depend on it"
    );

    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_droprole_grantee;
         CREATE TABLE acl_droprole_t (id int);
         GRANT SELECT ON acl_droprole_t TO acl_droprole_grantee;
         DROP ROLE acl_droprole_grantee;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "role \"acl_droprole_grantee\" cannot be dropped because some objects depend on it"
    );

    // The same role goes once the grant is revoked.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_droprole_ok;
             CREATE TABLE acl_droprole_t2 (id int);
             GRANT SELECT ON acl_droprole_t2 TO acl_droprole_ok;",
        ),
        (
            "0002.sql",
            "REVOKE SELECT ON acl_droprole_t2 FROM acl_droprole_ok;
             DROP ROLE acl_droprole_ok;",
        ),
    ]);
    assert!(!db.role_exists("acl_droprole_ok"));
}

#[test]
fn renames_keep_grants_and_drop_recreate_starts_fresh() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_rename_role;
             CREATE TABLE acl_rename_t (id int);
             GRANT SELECT ON acl_rename_t TO acl_rename_role;",
        ),
        (
            "0002.sql",
            "ALTER ROLE acl_rename_role RENAME TO acl_rename_role_new;
             ALTER TABLE acl_rename_t RENAME TO acl_rename_t_new;",
        ),
    ]);
    assert!(!db.role_exists("acl_rename_role"));
    assert!(db.role_exists("acl_rename_role_new"));
    assert!(table_has(
        &db,
        "acl_rename_role_new",
        "public",
        "acl_rename_t_new",
        Privilege::Select
    ));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_recreate_app;
             CREATE TABLE acl_recreate_t (id int);
             GRANT SELECT ON acl_recreate_t TO acl_recreate_app;",
        ),
        (
            "0002.sql",
            "DROP TABLE acl_recreate_t; CREATE TABLE acl_recreate_t (id int);",
        ),
    ]);
    assert!(!table_has(
        &db,
        "acl_recreate_app",
        "public",
        "acl_recreate_t",
        Privilege::Select
    ));
}

#[test]
fn views_and_materialized_views_carry_acls() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_view_app;
             CREATE TABLE acl_view_t (id int);
             CREATE VIEW acl_view_v AS SELECT id FROM acl_view_t;
             CREATE MATERIALIZED VIEW acl_view_m AS SELECT id FROM acl_view_t;",
        ),
        (
            "0002.sql",
            "GRANT SELECT ON acl_view_v, acl_view_m TO acl_view_app;",
        ),
    ]);
    assert!(table_has(
        &db,
        "acl_view_app",
        "public",
        "acl_view_v",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        "acl_view_app",
        "public",
        "acl_view_m",
        Privilege::Select
    ));
    assert!(!table_has(
        &db,
        "acl_view_app",
        "public",
        "acl_view_v",
        Privilege::Insert
    ));
}

#[test]
fn built_in_acls_come_from_the_seed() {
    let db = build_db(&[("0001.sql", "CREATE ROLE acl_builtin_app;")]);
    // Functions revoked from PUBLIC in a stock cluster.
    let read_file = db
        .find_functions(Some("pg_catalog"), "pg_read_file")
        .into_iter()
        .find(|p| p.proargtypes.len() == 1)
        .expect("pg_read_file(text)")
        .oid;
    assert!(!db.has_function_privilege("acl_builtin_app", read_file));
    assert!(db.has_function_privilege("postgres", read_file));
    // Restricted catalogs.
    let authid = relation(&db, "pg_catalog", "pg_authid");
    assert!(!db.has_table_privilege("acl_builtin_app", authid, Privilege::Select));
    assert!(db.has_table_privilege("postgres", authid, Privilege::Select));
    let statistic = relation(&db, "pg_catalog", "pg_statistic");
    assert!(!db.has_table_privilege("acl_builtin_app", statistic, Privilege::Select));
    // Executable by everyone.
    let now = proc_oid(&db, "pg_catalog", "now");
    assert!(db.has_function_privilege("acl_builtin_app", now));
    // The public schema is owned by pg_database_owner with USAGE to PUBLIC.
    let public = namespace(&db, "public");
    assert!(db.has_schema_privilege("acl_builtin_app", public, Privilege::Usage));
    assert!(!db.has_schema_privilege("acl_builtin_app", public, Privilege::Create));
}

#[test]
fn a_rolled_back_grant_leaves_no_privilege() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_tx_app;
             CREATE TABLE acl_tx_t (id int);",
        ),
        (
            "0002.sql",
            "BEGIN;
             GRANT SELECT ON acl_tx_t TO acl_tx_app;
             ROLLBACK;",
        ),
    ]);
    assert!(!table_has(
        &db,
        "acl_tx_app",
        "public",
        "acl_tx_t",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        MIGRATIONS_USER,
        "public",
        "acl_tx_t",
        Privilege::Select
    ));
}

#[test]
fn role_ddl_errors_match_postgres() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE ROLE acl_err_dup; CREATE ROLE acl_err_dup;"
        )]),
        DdlError::DuplicateObject(_),
        "role \"acl_err_dup\" already exists",
    );
    for (sql, message) in [
        (
            "CREATE ROLE pg_acl_reserved;",
            "role name \"pg_acl_reserved\" is reserved",
        ),
        ("CREATE ROLE public;", "role name \"public\" is reserved"),
        ("CREATE ROLE none;", "role name \"none\" is reserved"),
        (
            "DROP ROLE pg_acl_err_missing;",
            "role \"pg_acl_err_missing\" does not exist",
        ),
        (
            "CREATE ROLE acl_err_self; GRANT acl_err_self TO acl_err_self;",
            "role \"acl_err_self\" is a member of role \"acl_err_self\"",
        ),
        (
            "CREATE ROLE acl_err_cycle_a; CREATE ROLE acl_err_cycle_b;
             GRANT acl_err_cycle_a TO acl_err_cycle_b;
             GRANT acl_err_cycle_b TO acl_err_cycle_a;",
            "role \"acl_err_cycle_b\" is a member of role \"acl_err_cycle_a\"",
        ),
        (
            "CREATE ROLE acl_err_pgdb;
             GRANT acl_err_pgdb TO pg_database_owner;",
            "role \"pg_database_owner\" cannot be a member of any role",
        ),
        (
            "DROP ROLE public;",
            "cannot use special role specifier in DROP ROLE",
        ),
    ] {
        let result = try_apply(&[("0001.sql", sql)]);
        assert_err_prefix!(result, _, message);
    }
    assert_ddl_err!(
        try_apply(&[
            (
                "0001.sql",
                "CREATE ROLE acl_err_rename_a; CREATE ROLE acl_err_rename_b;"
            ),
            (
                "0002.sql",
                "ALTER ROLE acl_err_rename_a RENAME TO acl_err_rename_b;"
            ),
        ]),
        DdlError::DuplicateObject(_),
        "role \"acl_err_rename_b\" already exists",
    );

    // IF EXISTS suppresses the reserved-name error too.
    build_db(&[("0001.sql", "DROP ROLE IF EXISTS pg_acl_never_created;")]);
    assert_err_prefix!(
        try_apply(&[("0001.sql", "DROP ROLE pg_read_all_data;")]),
        DdlError::DependencyError(_),
        "cannot drop role pg_read_all_data because it is required by the database system"
    );
    assert_err_prefix!(
        try_apply(&[("0001.sql", "DROP ROLE postgres;")]),
        DdlError::Parse(_),
        "current user cannot be dropped"
    );
}

#[test]
fn column_revoke_and_dropped_columns() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_col_rev_app;
             CREATE TABLE acl_col_rev_t (a text, b text);
             GRANT SELECT (a), SELECT (b) ON acl_col_rev_t TO acl_col_rev_app;
             REVOKE SELECT (a) ON acl_col_rev_t FROM acl_col_rev_app;",
    )]);
    let table = relation(&db, "public", "acl_col_rev_t");
    assert!(!db.has_column_privilege(
        "acl_col_rev_app",
        table,
        attnum(&db, table, "a"),
        Privilege::Select
    ));
    assert!(db.has_column_privilege(
        "acl_col_rev_app",
        table,
        attnum(&db, table, "b"),
        Privilege::Select
    ));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_col_drop_app;
             CREATE TABLE acl_col_drop_t (a text, b text);
             GRANT SELECT (a) ON acl_col_drop_t TO acl_col_drop_app;",
        ),
        ("0002.sql", "ALTER TABLE acl_col_drop_t DROP COLUMN a;"),
    ]);
    let table = relation(&db, "public", "acl_col_drop_t");
    assert!(!db.has_any_column_privilege("acl_col_drop_app", table, Privilege::Select));
}

#[test]
fn set_schema_keeps_grants() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_setschema_app;
             CREATE SCHEMA acl_setschema_s;
             CREATE TABLE acl_setschema_t (id int);
             GRANT SELECT ON acl_setschema_t TO acl_setschema_app;",
        ),
        (
            "0002.sql",
            "ALTER TABLE acl_setschema_t SET SCHEMA acl_setschema_s;",
        ),
    ]);
    assert!(table_has(
        &db,
        "acl_setschema_app",
        "acl_setschema_s",
        "acl_setschema_t",
        Privilege::Select
    ));
}

#[test]
fn all_sequences_and_routines_in_schema() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_all_seq_app;
             CREATE SEQUENCE acl_all_seq_s1;
             CREATE SEQUENCE acl_all_seq_s2;
             GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO acl_all_seq_app;",
        ),
        ("0002.sql", "CREATE SEQUENCE acl_all_seq_s3;"),
    ]);
    assert!(db.has_sequence_privilege(
        "acl_all_seq_app",
        relation(&db, "public", "acl_all_seq_s1"),
        Privilege::Usage
    ));
    assert!(db.has_sequence_privilege(
        "acl_all_seq_app",
        relation(&db, "public", "acl_all_seq_s2"),
        Privilege::Usage
    ));
    assert!(!db.has_sequence_privilege(
        "acl_all_seq_app",
        relation(&db, "public", "acl_all_seq_s3"),
        Privilege::Usage
    ));

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_all_routine_app;
             CREATE FUNCTION acl_all_routine_f() RETURNS int LANGUAGE sql AS 'select 1';
             CREATE PROCEDURE acl_all_routine_p() LANGUAGE sql AS 'select 1';
             GRANT EXECUTE ON ALL ROUTINES IN SCHEMA public TO acl_all_routine_app;",
    )]);
    assert!(db.has_function_privilege(
        "acl_all_routine_app",
        proc_oid(&db, "public", "acl_all_routine_f")
    ));
    assert!(db.has_function_privilege(
        "acl_all_routine_app",
        proc_oid(&db, "public", "acl_all_routine_p")
    ));
}

#[test]
fn default_privileges_for_sequences_types_and_schemas() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_dp_seq_app;
             ALTER DEFAULT PRIVILEGES GRANT USAGE ON SEQUENCES TO acl_dp_seq_app;",
        ),
        ("0002.sql", "CREATE SEQUENCE acl_dp_seq_s;"),
    ]);
    assert!(db.has_sequence_privilege(
        "acl_dp_seq_app",
        relation(&db, "public", "acl_dp_seq_s"),
        Privilege::Usage
    ));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_dp_type_app;
             ALTER DEFAULT PRIVILEGES GRANT USAGE ON TYPES TO acl_dp_type_app;",
        ),
        ("0002.sql", "CREATE TYPE acl_dp_type_e AS ENUM ('a');"),
    ]);
    assert!(db.has_type_privilege("acl_dp_type_app", type_oid(&db, "public", "acl_dp_type_e")));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_dp_schema_owner;
             CREATE ROLE acl_dp_schema_app;
             ALTER DEFAULT PRIVILEGES FOR ROLE acl_dp_schema_owner
                 GRANT USAGE ON SCHEMAS TO acl_dp_schema_app;",
        ),
        (
            "0002.sql",
            "CREATE SCHEMA acl_dp_schema_created AUTHORIZATION acl_dp_schema_owner;",
        ),
    ]);
    assert!(db.has_schema_privilege(
        "acl_dp_schema_app",
        namespace(&db, "acl_dp_schema_created"),
        Privilege::Usage
    ));

    // Default privileges recorded by a `SET ROLE` session belong to that
    // role.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_dp_current_owner;
             CREATE ROLE acl_dp_current_app;
             CREATE SCHEMA acl_dp_current_s AUTHORIZATION acl_dp_current_owner;
             SET ROLE acl_dp_current_owner;
             ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO acl_dp_current_app;
             CREATE TABLE acl_dp_current_s.t (id int);
             RESET ROLE;",
    )]);
    assert!(table_has(
        &db,
        "acl_dp_current_app",
        "acl_dp_current_s",
        "t",
        Privilege::Select
    ));
}

#[test]
fn membership_admin_option_and_revoke() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_admin_granted;
             CREATE ROLE acl_admin_member;
             GRANT acl_admin_granted TO acl_admin_member WITH ADMIN OPTION;",
        ),
        (
            "0002.sql",
            "REVOKE ADMIN OPTION FOR acl_admin_granted FROM acl_admin_member;",
        ),
    ]);
    assert!(db.role_exists("acl_admin_member"));
}

#[test]
fn drop_role_if_exists_and_drop_owned_removes_objects() {
    let db = build_db(&[("0001.sql", "DROP ROLE IF EXISTS acl_never_created;")]);
    assert!(!db.role_exists("acl_never_created"));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_drop_obj_owner;
             CREATE SCHEMA acl_drop_obj_s AUTHORIZATION acl_drop_obj_owner;
             SET ROLE acl_drop_obj_owner;
             CREATE TABLE acl_drop_obj_s.t (id int);
             CREATE FUNCTION acl_drop_obj_s.f() RETURNS int LANGUAGE sql AS 'select 1';
             RESET ROLE;",
        ),
        ("0002.sql", "DROP OWNED BY acl_drop_obj_owner;"),
    ]);
    assert!(
        db.resolve_table(Some("acl_drop_obj_s"), "t").is_none()
            || db.pg_class().values().all(|c| c.relname != "t")
    );
    assert!(db.namespace_oid("acl_drop_obj_s").is_none());
    assert!(!db.pg_proc().values().any(|p| p.proname == "f"));
}

#[test]
fn create_or_replace_keeps_ownership_and_acl() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_replace_app;
             CREATE FUNCTION acl_replace_f() RETURNS int LANGUAGE sql AS 'select 1';
             REVOKE EXECUTE ON FUNCTION acl_replace_f() FROM PUBLIC;
             CREATE OR REPLACE FUNCTION acl_replace_f() RETURNS int LANGUAGE sql AS 'select 2';
             CREATE TABLE acl_replace_t (id int);
             CREATE VIEW acl_replace_v AS SELECT id FROM acl_replace_t;
             GRANT SELECT ON acl_replace_v TO acl_replace_app;
             CREATE OR REPLACE VIEW acl_replace_v AS SELECT id FROM acl_replace_t;",
    )]);
    assert!(
        !db.has_function_privilege("acl_replace_app", proc_oid(&db, "public", "acl_replace_f"))
    );
    assert!(db.has_function_privilege(MIGRATIONS_USER, proc_oid(&db, "public", "acl_replace_f")));
    assert!(table_has(
        &db,
        "acl_replace_app",
        "public",
        "acl_replace_v",
        Privilege::Select
    ));
}

#[test]
fn transitive_membership_revoke_cuts_the_chain() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_chain_g;
             CREATE ROLE acl_chain_m1;
             CREATE ROLE acl_chain_m2;
             CREATE TABLE acl_chain_t (id int);
             GRANT SELECT ON acl_chain_t TO acl_chain_g;
             GRANT acl_chain_g TO acl_chain_m1;
             GRANT acl_chain_m1 TO acl_chain_m2;",
        ),
        ("0002.sql", "REVOKE acl_chain_m1 FROM acl_chain_m2;"),
    ]);
    assert!(!table_has(
        &db,
        "acl_chain_m2",
        "public",
        "acl_chain_t",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        "acl_chain_m1",
        "public",
        "acl_chain_t",
        Privilege::Select
    ));
}

#[test]
fn predefined_roles_reach_sequences_and_schemas() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_pre_seq_read;
         CREATE ROLE acl_pre_seq_write;
         CREATE SEQUENCE acl_pre_seq_s;
         GRANT pg_read_all_data TO acl_pre_seq_read;
         GRANT pg_write_all_data TO acl_pre_seq_write;",
    )]);
    let sequence = relation(&db, "public", "acl_pre_seq_s");
    assert!(db.has_sequence_privilege("acl_pre_seq_read", sequence, Privilege::Select));
    assert!(!db.has_sequence_privilege("acl_pre_seq_read", sequence, Privilege::Usage));
    assert!(db.has_sequence_privilege("acl_pre_seq_write", sequence, Privilege::Update));
    assert!(!db.has_sequence_privilege("acl_pre_seq_write", sequence, Privilege::Select));
    assert!(db.has_schema_privilege(
        "acl_pre_seq_write",
        namespace(&db, "public"),
        Privilege::Usage
    ));
}

#[test]
fn alter_owner_rejects_public() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE acl_err_owner_t (id int);"),
        ("0002.sql", "ALTER TABLE acl_err_owner_t OWNER TO PUBLIC;"),
    ]);
    assert_err_prefix!(
        result,
        DdlError::TypeNotFound(_),
        "role \"public\" does not exist"
    );
}

#[test]
fn default_privileges_are_role_dependencies() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_dp_dep;
         ALTER DEFAULT PRIVILEGES FOR ROLE acl_dp_dep
             GRANT SELECT ON TABLES TO PUBLIC;
         DROP ROLE acl_dp_dep;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "role \"acl_dp_dep\" cannot be dropped because some objects depend on it"
    );

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_dp_dep_ok;
             ALTER DEFAULT PRIVILEGES FOR ROLE acl_dp_dep_ok
                 GRANT SELECT ON TABLES TO PUBLIC;",
        ),
        (
            "0002.sql",
            "DROP OWNED BY acl_dp_dep_ok;
             DROP ROLE acl_dp_dep_ok;",
        ),
    ]);
    assert!(!db.role_exists("acl_dp_dep_ok"));
}

#[test]
fn a_rolled_back_create_role_disappears() {
    let db = build_db(&[
        ("0001.sql", "CREATE TABLE acl_tx_role_t (id int);"),
        (
            "0002.sql",
            "BEGIN; CREATE ROLE acl_tx_rolled_back; ROLLBACK;",
        ),
    ]);
    assert!(!db.role_exists("acl_tx_rolled_back"));
}

#[test]
fn role_options_are_accepted() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_opts_role LOGIN PASSWORD 'secret'
             VALID UNTIL '2030-01-01' CONNECTION LIMIT 5
             CREATEDB CREATEROLE REPLICATION BYPASSRLS;
         CREATE USER acl_opts_user PASSWORD NULL;
         CREATE GROUP acl_opts_group;
         ALTER ROLE acl_opts_role NOLOGIN NOBYPASSRLS PASSWORD 'other';",
    )]);
    assert!(db.role_exists("acl_opts_role"));
    assert!(db.role_exists("acl_opts_user"));
    assert!(db.role_exists("acl_opts_group"));
}

#[test]
fn owner_change_rewrites_existing_grants() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_owner_acl_old;
             CREATE ROLE acl_owner_acl_new;
             CREATE ROLE acl_owner_acl_app;
             CREATE SCHEMA acl_owner_acl_s AUTHORIZATION acl_owner_acl_old;
             SET ROLE acl_owner_acl_old;
             CREATE TABLE acl_owner_acl_s.t (id int);
             RESET ROLE;
             GRANT SELECT ON acl_owner_acl_s.t TO acl_owner_acl_app;",
        ),
        (
            "0002.sql",
            "ALTER TABLE acl_owner_acl_s.t OWNER TO acl_owner_acl_new;",
        ),
    ]);
    let table = relation(&db, "acl_owner_acl_s", "t");
    assert!(!db.has_table_privilege("acl_owner_acl_old", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_owner_acl_new", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_owner_acl_app", table, Privilege::Select));

    // The rewritten grantor means the new owner's REVOKE removes the grant.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_owner_acl_old2;
             CREATE ROLE acl_owner_acl_new2;
             CREATE ROLE acl_owner_acl_app2;
             CREATE SCHEMA acl_owner_acl_s2 AUTHORIZATION acl_owner_acl_old2;
             SET ROLE acl_owner_acl_old2;
             CREATE TABLE acl_owner_acl_s2.t (id int);
             RESET ROLE;
             GRANT USAGE ON SCHEMA acl_owner_acl_s2 TO acl_owner_acl_new2;
             GRANT SELECT ON acl_owner_acl_s2.t TO acl_owner_acl_app2;",
        ),
        (
            "0002.sql",
            "ALTER TABLE acl_owner_acl_s2.t OWNER TO acl_owner_acl_new2;
             SET ROLE acl_owner_acl_new2;
             REVOKE SELECT ON acl_owner_acl_s2.t FROM acl_owner_acl_app2;
             RESET ROLE;",
        ),
    ]);
    assert!(!db.has_table_privilege(
        "acl_owner_acl_app2",
        relation(&db, "acl_owner_acl_s2", "t"),
        Privilege::Select
    ));
}

#[test]
fn reassign_rewrites_the_grantor() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_reassign_grantor_old;
             CREATE ROLE acl_reassign_grantor_new;
             CREATE ROLE acl_reassign_grantor_app;
             CREATE SCHEMA acl_reassign_grantor_s AUTHORIZATION acl_reassign_grantor_old;
             SET ROLE acl_reassign_grantor_old;
             CREATE TABLE acl_reassign_grantor_s.t (id int);
             RESET ROLE;
             GRANT SELECT ON acl_reassign_grantor_s.t TO acl_reassign_grantor_app;",
        ),
        (
            "0002.sql",
            "REASSIGN OWNED BY acl_reassign_grantor_old TO acl_reassign_grantor_new;",
        ),
        (
            "0003.sql",
            "SET ROLE acl_reassign_grantor_new;
             REVOKE SELECT ON acl_reassign_grantor_s.t FROM acl_reassign_grantor_app;
             RESET ROLE;",
        ),
    ]);
    assert!(!db.has_table_privilege(
        "acl_reassign_grantor_app",
        relation(&db, "acl_reassign_grantor_s", "t"),
        Privilege::Select
    ));
}

#[test]
fn default_privileges_can_be_revoked_again() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_dp_revoke_app;
             ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO acl_dp_revoke_app;
             ALTER DEFAULT PRIVILEGES REVOKE SELECT ON TABLES FROM acl_dp_revoke_app;",
        ),
        ("0002.sql", "CREATE TABLE acl_dp_revoke_t (id int);"),
    ]);
    assert!(!table_has(
        &db,
        "acl_dp_revoke_app",
        "public",
        "acl_dp_revoke_t",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        MIGRATIONS_USER,
        "public",
        "acl_dp_revoke_t",
        Privilege::Select
    ));
}

#[test]
fn has_table_privilege_also_answers_for_sequences() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_seq_table_has_app;
         CREATE SEQUENCE acl_seq_table_s;
         GRANT SELECT ON acl_seq_table_s TO acl_seq_table_has_app;",
    )]);
    let sequence = relation(&db, "public", "acl_seq_table_s");
    assert!(db.has_table_privilege("acl_seq_table_has_app", sequence, Privilege::Select));
    assert!(!db.has_table_privilege("acl_seq_table_has_app", sequence, Privilege::Update));
    assert!(db.has_sequence_privilege("acl_seq_table_has_app", sequence, Privilege::Select));
}

#[test]
fn create_role_membership_options() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_in_parent;
         CREATE ROLE acl_in_child IN ROLE acl_in_parent;
         CREATE ROLE acl_role_parent;
         CREATE ROLE acl_role_child ROLE acl_role_parent;
         CREATE ROLE acl_admin_parent;
         CREATE ROLE acl_admin_child ADMIN acl_admin_parent;
         CREATE TABLE acl_in_t (id int);
         GRANT SELECT ON acl_in_t TO acl_in_parent, acl_role_child, acl_admin_child;",
        ),
        (
            "0002.sql",
            "REVOKE acl_in_parent FROM acl_in_child;
         REVOKE acl_role_child FROM acl_role_parent;",
        ),
    ]);
    // IN ROLE: the new role joins the named one; the migrations user's
    // REVOKE takes that membership back.
    assert!(!table_has(
        &db,
        "acl_in_child",
        "public",
        "acl_in_t",
        Privilege::Select
    ));
    // ROLE / ADMIN: the named role joins the new one.
    assert!(!table_has(
        &db,
        "acl_role_parent",
        "public",
        "acl_in_t",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        "acl_admin_parent",
        "public",
        "acl_in_t",
        Privilege::Select
    ));
}

#[test]
fn drop_role_refuses_a_role_that_granted_privileges() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_grantor_role;
         CREATE ROLE acl_grantor_app;
         CREATE TABLE acl_grantor_t (id int);
         GRANT SELECT ON acl_grantor_t TO acl_grantor_role WITH GRANT OPTION;
         SET ROLE acl_grantor_role;
         GRANT SELECT ON acl_grantor_t TO acl_grantor_app;
         RESET ROLE;
         DROP ROLE acl_grantor_role;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "role \"acl_grantor_role\" cannot be dropped because some objects depend on it"
    );

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_grantor_ok;
             CREATE ROLE acl_grantor_ok_app;
             CREATE TABLE acl_grantor_ok_t (id int);
             GRANT SELECT ON acl_grantor_ok_t TO acl_grantor_ok WITH GRANT OPTION;
             SET ROLE acl_grantor_ok;
             GRANT SELECT ON acl_grantor_ok_t TO acl_grantor_ok_app;
             RESET ROLE;",
        ),
        (
            "0002.sql",
            "DROP OWNED BY acl_grantor_ok;
             DROP ROLE acl_grantor_ok;",
        ),
    ]);
    assert!(!db.role_exists("acl_grantor_ok"));
    assert!(!table_has(
        &db,
        "acl_grantor_ok_app",
        "public",
        "acl_grantor_ok_t",
        Privilege::Select
    ));
}

#[test]
fn serial_sequences_of_a_role_created_table_belong_to_it() {
    // `transformColumnDefinition` / `generateSerialExtraStmts` create a
    // table's serial and identity sequences before the table's owner is
    // recorded; they still belong to the role creating the table, with that
    // role's sequence default privileges.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_serial_role;
         CREATE ROLE acl_serial_app;
         ALTER DEFAULT PRIVILEGES FOR ROLE acl_serial_role
             GRANT SELECT ON SEQUENCES TO acl_serial_app;
         CREATE SCHEMA acl_serial_s AUTHORIZATION acl_serial_role;
         SET ROLE acl_serial_role;
         CREATE TABLE acl_serial_s.t (id serial, ident int GENERATED ALWAYS AS IDENTITY);
         RESET ROLE;",
    )]);
    for name in ["t_id_seq", "t_ident_seq"] {
        let sequence = relation(&db, "acl_serial_s", name);
        assert!(
            db.has_sequence_privilege("acl_serial_role", sequence, Privilege::Usage),
            "the sequence {name} belongs to the role that owns the table"
        );
        assert!(
            db.has_sequence_privilege("acl_serial_app", sequence, Privilege::Select),
            "the sequence {name} takes its owner's default privileges"
        );
        assert!(!db.has_sequence_privilege("acl_serial_app", sequence, Privilege::Update));
    }
    assert!(db.has_sequence_privilege(
        MIGRATIONS_USER,
        relation(&db, "acl_serial_s", "t_id_seq"),
        Privilege::Usage
    ));
}

#[test]
fn schema_elements_are_created_as_the_schema_owner() {
    // CreateSchemaCommand temporarily becomes the schema's owner
    // (`SetUserIdAndSecContext`), so its elements are the owner's — and take
    // the owner's default privileges.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_schema_el_owner;
         CREATE ROLE acl_schema_el_app;
         CREATE ROLE acl_schema_el_other;
         ALTER DEFAULT PRIVILEGES FOR ROLE acl_schema_el_owner
             GRANT SELECT ON TABLES TO acl_schema_el_app;
         CREATE SCHEMA acl_schema_el AUTHORIZATION acl_schema_el_owner
             CREATE TABLE t (id int)
             CREATE VIEW v AS SELECT id FROM t;",
    )]);
    for name in ["t", "v"] {
        assert!(table_has(
            &db,
            "acl_schema_el_owner",
            "acl_schema_el",
            name,
            Privilege::Select
        ));
        assert!(
            table_has(
                &db,
                "acl_schema_el_app",
                "acl_schema_el",
                name,
                Privilege::Select
            ),
            "the schema element {name} gets the owner's default privileges"
        );
        assert!(!table_has(
            &db,
            "acl_schema_el_other",
            "acl_schema_el",
            name,
            Privilege::Insert
        ));
    }
}

#[test]
fn drop_owned_revokes_only_the_owners_grants() {
    // DROP OWNED issues `REVOKE ALL FROM role` as each object's owner
    // (RemoveRoleFromObjectACL), so a grant another grantor made to the role
    // survives.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_do_owner;
             CREATE ROLE acl_do_grantor;
             CREATE ROLE acl_do_r;
             CREATE SCHEMA acl_do_s AUTHORIZATION acl_do_owner;
             GRANT USAGE ON SCHEMA acl_do_s TO acl_do_grantor;
             SET ROLE acl_do_owner;
             CREATE TABLE acl_do_s.t (id int);
             GRANT SELECT ON acl_do_s.t TO acl_do_grantor WITH GRANT OPTION;
             GRANT INSERT ON acl_do_s.t TO acl_do_r;
             RESET ROLE;
             SET ROLE acl_do_grantor;
             GRANT SELECT ON acl_do_s.t TO acl_do_r;
             RESET ROLE;
             CREATE ROLE acl_do_membership;
             CREATE ROLE acl_do_member;
             CREATE TABLE acl_do_membership_t (id int);
             GRANT SELECT ON acl_do_membership_t TO acl_do_membership;
             GRANT acl_do_membership TO acl_do_r WITH ADMIN OPTION;
             SET ROLE acl_do_r;
             GRANT acl_do_membership TO acl_do_member;
             RESET ROLE;",
        ),
        ("0002.sql", "DROP OWNED BY acl_do_r;"),
    ]);
    let table = relation(&db, "acl_do_s", "t");
    assert!(db.has_table_privilege("acl_do_r", table, Privilege::Select));
    assert!(!db.has_table_privilege("acl_do_r", table, Privilege::Insert));
    // The membership the role granted goes with it.
    assert!(!table_has(
        &db,
        "acl_do_member",
        "public",
        "acl_do_membership_t",
        Privilege::Select
    ));
}

#[test]
fn drop_owned_restrict_fails_on_other_roles_dependents() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_do2_owner;
         CREATE ROLE acl_do2_viewer;
         CREATE SCHEMA acl_do2_s AUTHORIZATION acl_do2_owner;
         SET ROLE acl_do2_owner;
         CREATE TABLE acl_do2_s.t (id int);
         RESET ROLE;
         GRANT USAGE, CREATE ON SCHEMA acl_do2_s TO acl_do2_viewer;
         GRANT SELECT ON acl_do2_s.t TO acl_do2_viewer;
         SET ROLE acl_do2_viewer;
         CREATE VIEW acl_do2_s.v AS SELECT id FROM acl_do2_s.t;
         RESET ROLE;
         DROP OWNED BY acl_do2_owner;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "cannot drop desired object(s) because other objects depend on them (view \
         acl_do2_s.v depends on table acl_do2_s.t)"
    );
}

#[test]
fn drop_owned_cascade_drops_other_roles_dependents() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_do3_owner;
         CREATE ROLE acl_do3_viewer;
         CREATE SCHEMA acl_do3_s AUTHORIZATION acl_do3_owner;
         SET ROLE acl_do3_owner;
         CREATE TABLE acl_do3_s.t (id int);
         RESET ROLE;
         GRANT USAGE, CREATE ON SCHEMA acl_do3_s TO acl_do3_viewer;
         GRANT SELECT ON acl_do3_s.t TO acl_do3_viewer;
         SET ROLE acl_do3_viewer;
         CREATE VIEW acl_do3_s.v AS SELECT id FROM acl_do3_s.t;
         RESET ROLE;
         DROP OWNED BY acl_do3_owner CASCADE;",
    )]);
    assert!(db.namespace_oid("acl_do3_s").is_none());
    assert!(db.resolve_table(Some("acl_do3_s"), "t").is_none());
    assert!(db.resolve_table(Some("acl_do3_s"), "v").is_none());
}

#[test]
fn role_granted_by_records_and_revokes_the_named_grantor() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_gb_g;
         CREATE ROLE acl_gb_grantor;
         CREATE ROLE acl_gb_app;
         CREATE TABLE acl_gb_t (id int);
         GRANT SELECT ON acl_gb_t TO acl_gb_g;
         GRANT acl_gb_g TO acl_gb_grantor WITH ADMIN OPTION;
         GRANT acl_gb_g TO acl_gb_app GRANTED BY acl_gb_grantor;
         REVOKE acl_gb_g FROM acl_gb_app;",
    )]);
    // The superuser's REVOKE looks for its own grant (the bootstrap
    // superuser's) and finds none: nothing changes.
    assert!(table_has(
        &db,
        "acl_gb_app",
        "public",
        "acl_gb_t",
        Privilege::Select
    ));

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_gb2_g;
             CREATE ROLE acl_gb2_grantor;
             CREATE ROLE acl_gb2_app;
             CREATE TABLE acl_gb2_t (id int);
             GRANT SELECT ON acl_gb2_t TO acl_gb2_g;
             GRANT acl_gb2_g TO acl_gb2_grantor WITH ADMIN OPTION;
             GRANT acl_gb2_g TO acl_gb2_app GRANTED BY acl_gb2_grantor;",
        ),
        (
            "0002.sql",
            "REVOKE acl_gb2_g FROM acl_gb2_app GRANTED BY acl_gb2_grantor;",
        ),
    ]);
    assert!(!table_has(
        &db,
        "acl_gb2_app",
        "public",
        "acl_gb2_t",
        Privilege::Select
    ));
}

#[test]
fn role_grant_requires_the_grantor_to_hold_admin() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_gb3_g;
         CREATE ROLE acl_gb3_grantor;
         CREATE ROLE acl_gb3_app;
         GRANT acl_gb3_g TO acl_gb3_grantor;
         GRANT acl_gb3_g TO acl_gb3_app GRANTED BY acl_gb3_grantor;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::Parse(_),
        "permission denied to grant privileges as role \"acl_gb3_grantor\""
    );
}

#[test]
fn revoking_role_membership_with_dependents_needs_cascade() {
    let setup = "CREATE ROLE acl_admin_g;
         CREATE ROLE acl_admin_middle;
         CREATE ROLE acl_admin_app;
         CREATE TABLE acl_admin_t (id int);
         GRANT SELECT ON acl_admin_t TO acl_admin_g;
         GRANT acl_admin_g TO acl_admin_middle WITH ADMIN OPTION;
         SET ROLE acl_admin_middle;
         GRANT acl_admin_g TO acl_admin_app;
         RESET ROLE;";
    let result = try_apply(&[(
        "0001.sql",
        &format!(
            "{setup}
             REVOKE ADMIN OPTION FOR acl_admin_g FROM acl_admin_middle;"
        ),
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "dependent privileges exist"
    );

    let db = build_db(&[(
        "0001.sql",
        &format!(
            "{setup}
             REVOKE ADMIN OPTION FOR acl_admin_g FROM acl_admin_middle CASCADE;"
        ),
    )]);
    assert!(!table_has(
        &db,
        "acl_admin_app",
        "public",
        "acl_admin_t",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        "acl_admin_middle",
        "public",
        "acl_admin_t",
        Privilege::Select
    ));
}

#[test]
fn role_revoke_with_cascade_keeps_grants_backed_by_a_surviving_admin() {
    // PG's plan_recursive_revoke only descends when the member would no
    // longer have ADMIN OPTION through any surviving grant.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_rr_g;
         CREATE ROLE acl_rr_a;
         CREATE ROLE acl_rr_b;
         CREATE ROLE acl_rr_app;
         CREATE TABLE acl_rr_t (id int);
         GRANT SELECT ON acl_rr_t TO acl_rr_g;
         GRANT acl_rr_g TO acl_rr_a WITH ADMIN OPTION;
         SET ROLE acl_rr_a;
         GRANT acl_rr_g TO acl_rr_b WITH ADMIN OPTION;
         SET ROLE acl_rr_b;
         GRANT acl_rr_g TO acl_rr_app;
         RESET ROLE;
         GRANT acl_rr_g TO acl_rr_b WITH ADMIN OPTION;
         REVOKE acl_rr_g FROM acl_rr_b CASCADE;",
    )]);
    assert!(table_has(
        &db,
        "acl_rr_app",
        "public",
        "acl_rr_t",
        Privilege::Select
    ));
    assert!(db.role_exists("acl_rr_app"));
}

#[test]
fn grant_through_an_inherited_grant_option_records_the_inherited_role() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_bg_group;
             CREATE ROLE acl_bg_member;
             CREATE ROLE acl_bg_app;
             CREATE TABLE acl_bg_t (id int);
             GRANT SELECT ON acl_bg_t TO acl_bg_group WITH GRANT OPTION;
             GRANT acl_bg_group TO acl_bg_member;
             SET ROLE acl_bg_member;
             GRANT SELECT ON acl_bg_t TO acl_bg_app;
             RESET ROLE;",
        ),
        (
            "0002.sql",
            "REVOKE SELECT ON acl_bg_t FROM acl_bg_group CASCADE;",
        ),
    ]);
    assert!(!table_has(
        &db,
        "acl_bg_app",
        "public",
        "acl_bg_t",
        Privilege::Select
    ));
}

#[test]
fn grants_and_revokes_are_limited_to_the_grantors_grant_options() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_po_g;
             CREATE ROLE acl_po_app;
             CREATE TABLE acl_po_t (id int);
             GRANT INSERT ON acl_po_t TO acl_po_app;
             GRANT SELECT ON acl_po_t TO acl_po_g WITH GRANT OPTION;
             SET ROLE acl_po_g;
             GRANT ALL PRIVILEGES ON acl_po_t TO acl_po_app;
             RESET ROLE;",
    )]);
    let table = relation(&db, "public", "acl_po_t");
    // The grantor only holds SELECT WITH GRANT OPTION: GRANT ALL only grants
    // SELECT (restrict_and_check_grant), and the owner's INSERT survives.
    assert!(db.has_table_privilege("acl_po_app", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_po_app", table, Privilege::Insert));
    for privilege in [
        Privilege::Update,
        Privilege::Delete,
        Privilege::Truncate,
        Privilege::References,
        Privilege::Trigger,
        Privilege::Maintain,
    ] {
        assert!(
            !db.has_table_privilege("acl_po_app", table, privilege),
            "grantor without the {privilege:?} grant option cannot grant it"
        );
    }

    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_po2_g;
             CREATE ROLE acl_po2_app;
             CREATE TABLE acl_po2_t (id int);
             GRANT INSERT ON acl_po2_t TO acl_po2_app;
             GRANT SELECT ON acl_po2_t TO acl_po2_g WITH GRANT OPTION;
             SET ROLE acl_po2_g;
             GRANT ALL PRIVILEGES ON acl_po2_t TO acl_po2_app;
             RESET ROLE;",
        ),
        (
            "0002.sql",
            "SET ROLE acl_po2_g;
             REVOKE ALL PRIVILEGES ON acl_po2_t FROM acl_po2_app;
             RESET ROLE;",
        ),
    ]);
    let table = relation(&db, "public", "acl_po2_t");
    // The REVOKE could only revoke what the grantor granted: its SELECT.
    assert!(!db.has_table_privilege("acl_po2_app", table, Privilege::Select));
    assert!(db.has_table_privilege("acl_po2_app", table, Privilege::Insert));
}

#[test]
fn the_owner_keeps_grant_options_through_membership() {
    // recursive_revoke's aclmask treats a member of the owner as holding
    // every grant option, so revoking the grant does not cascade into the
    // grants it made.
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_og_owner;
             CREATE ROLE acl_og_holder;
             CREATE ROLE acl_og_app;
             GRANT acl_og_owner TO acl_og_holder;
             CREATE SCHEMA acl_og_s AUTHORIZATION acl_og_owner;
             SET ROLE acl_og_owner;
             CREATE TABLE acl_og_s.t (id int);
             GRANT SELECT ON acl_og_s.t TO acl_og_holder WITH GRANT OPTION;
             RESET ROLE;
             SET ROLE acl_og_holder;
             GRANT SELECT ON acl_og_s.t TO acl_og_app;
             RESET ROLE;",
        ),
        (
            "0002.sql",
            "REVOKE SELECT ON acl_og_s.t FROM acl_og_holder CASCADE;",
        ),
    ]);
    assert!(table_has(
        &db,
        "acl_og_app",
        "acl_og_s",
        "t",
        Privilege::Select
    ));
    assert!(table_has(
        &db,
        "acl_og_holder",
        "acl_og_s",
        "t",
        Privilege::Select
    ));
}

#[test]
fn added_columns_take_their_tables_owners_default_privileges() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl_addcol_owner;
             CREATE ROLE acl_addcol_cur;
             CREATE ROLE acl_addcol_app;
             CREATE SCHEMA acl_addcol_s AUTHORIZATION acl_addcol_owner;
             GRANT USAGE, CREATE ON SCHEMA acl_addcol_s TO acl_addcol_cur;
             SET ROLE acl_addcol_owner;
             CREATE TABLE acl_addcol_s.t (a int);
             RESET ROLE;
             ALTER DEFAULT PRIVILEGES FOR ROLE acl_addcol_cur
                 GRANT SELECT ON SEQUENCES TO acl_addcol_app;
             GRANT acl_addcol_owner TO acl_addcol_cur;",
        ),
        (
            "0002.sql",
            "SET ROLE acl_addcol_cur;
             ALTER TABLE acl_addcol_s.t ADD COLUMN id serial;
             RESET ROLE;",
        ),
    ]);
    let sequence = relation(&db, "acl_addcol_s", "t_id_seq");
    // The sequence belongs to the table's owner and takes *its* (empty)
    // default privileges, not the current role's.
    assert!(db.has_sequence_privilege("acl_addcol_owner", sequence, Privilege::Usage));
    assert!(!db.has_sequence_privilege("acl_addcol_app", sequence, Privilege::Select));
}

#[test]
fn drop_role_details_name_default_privileges_and_memberships() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_dep_grantee;
         CREATE ROLE acl_dep_owner;
         ALTER DEFAULT PRIVILEGES FOR ROLE acl_dep_owner
             GRANT SELECT ON TABLES TO acl_dep_grantee;
         DROP ROLE acl_dep_grantee;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "role \"acl_dep_grantee\" cannot be dropped because some objects depend on it \
         (privileges for default privileges on new relations belonging to role acl_dep_owner)"
    );

    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_dep2_g;
         CREATE ROLE acl_dep2_member;
         CREATE ROLE acl_dep2_grantor;
         GRANT acl_dep2_g TO acl_dep2_member WITH ADMIN OPTION;
         GRANT acl_dep2_g TO acl_dep2_grantor WITH ADMIN OPTION;
         GRANT acl_dep2_g TO acl_dep2_member GRANTED BY acl_dep2_grantor;
         DROP ROLE acl_dep2_grantor;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "role \"acl_dep2_grantor\" cannot be dropped because some objects depend on it \
         (privileges for membership of role acl_dep2_member in role acl_dep2_g)"
    );
}

#[test]
fn pg_database_owner_cannot_have_members() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_pdo_member;
         GRANT pg_database_owner TO acl_pdo_member;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::Parse(_),
        "role \"pg_database_owner\" cannot have explicit members"
    );
}

#[test]
fn range_constructors_take_the_bootstrap_default_privileges() {
    // makeRangeConstructors passes BOOTSTRAP_SUPERUSERID as proowner, so the
    // constructors take that role's function default privileges — not the
    // migration user's (the model keeps the two identities apart).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_range_app;
         CREATE ROLE acl_range_other;
         ALTER DEFAULT PRIVILEGES FOR ROLE postgres
             REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES FOR ROLE postgres
             GRANT EXECUTE ON FUNCTIONS TO acl_range_app;
         CREATE TYPE acl_rng_r AS RANGE (subtype = int4);",
    )]);
    for name in ["acl_rng_r", "acl_rng_r_multirange"] {
        let constructors = db.find_functions(Some("public"), name);
        assert!(!constructors.is_empty(), "the {name} constructors exist");
        for constructor in constructors {
            assert!(
                db.has_function_privilege("acl_range_app", constructor.oid),
                "the {name} constructor takes the bootstrap function defaults"
            );
            assert!(
                !db.has_function_privilege("acl_range_other", constructor.oid),
                "PUBLIC's EXECUTE was revoked in the bootstrap defaults"
            );
        }
    }
}

#[test]
fn granting_on_table_restricts_to_the_sequences_privileges() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_seqgrant_app;
         CREATE SEQUENCE acl_seqgrant_s;
         GRANT INSERT, SELECT ON TABLE acl_seqgrant_s TO acl_seqgrant_app;
         GRANT INSERT, USAGE ON TABLE acl_seqgrant_s TO acl_seqgrant_app WITH GRANT OPTION;",
    )]);
    let sequence = relation(&db, "public", "acl_seqgrant_s");
    assert!(db.has_sequence_privilege("acl_seqgrant_app", sequence, Privilege::Select));
    assert!(db.has_sequence_privilege("acl_seqgrant_app", sequence, Privilege::Usage));
    assert!(!db.has_table_privilege("acl_seqgrant_app", sequence, Privilege::Insert));
    assert!(!db.has_table_privilege_with_grant_option(
        "acl_seqgrant_app",
        sequence,
        Privilege::Insert
    ));
    assert!(db.has_sequence_privilege_with_grant_option(
        "acl_seqgrant_app",
        sequence,
        Privilege::Usage
    ));
    assert!(!db.has_sequence_privilege_with_grant_option(
        "acl_seqgrant_app",
        sequence,
        Privilege::Select
    ));
    assert!(!db.has_sequence_privilege("acl_seqgrant_app", sequence, Privilege::Update));
}

#[test]
fn alter_sequence_owner_refuses_a_linked_sequence() {
    for sql in [
        "ALTER SEQUENCE acl_link_t_id_seq OWNER TO acl_link_r;",
        "ALTER TABLE acl_link_t_id_seq OWNER TO acl_link_r;",
    ] {
        let result = try_apply(&[(
            "0001.sql",
            &format!(
                "CREATE ROLE acl_link_r;
                 CREATE TABLE acl_link_t (id serial);
                 {sql}"
            ),
        )]);
        assert_err_prefix!(
            result,
            DdlError::UnsupportedDdl(_),
            "cannot change owner of sequence \"acl_link_t_id_seq\" (Sequence \
             \"acl_link_t_id_seq\" is linked to table \"acl_link_t\".)"
        );
    }
    for sql in [
        "ALTER SEQUENCE acl_link2_t_id_seq OWNER TO acl_link2_r;",
        "ALTER TABLE acl_link2_t_id_seq OWNER TO acl_link2_r;",
    ] {
        let result = try_apply(&[(
            "0001.sql",
            &format!(
                "CREATE ROLE acl_link2_r;
                 CREATE TABLE acl_link2_t (id int GENERATED ALWAYS AS IDENTITY);
                 {sql}"
            ),
        )]);
        assert_err_prefix!(
            result,
            DdlError::UnsupportedDdl(_),
            "cannot change owner of sequence \"acl_link2_t_id_seq\""
        );
    }
    // The owning table can still be given away, taking the sequence along.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_link3_r;
         CREATE TABLE acl_link3_t (id serial);
         ALTER TABLE acl_link3_t OWNER TO acl_link3_r;",
    )]);
    assert!(db.has_sequence_privilege(
        "acl_link3_r",
        relation(&db, "public", "acl_link3_t_id_seq"),
        Privilege::Usage
    ));
}

#[test]
fn create_schema_authorization_current_user_uses_its_name() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_cs_user SUPERUSER;
         SET ROLE acl_cs_user;
         CREATE SCHEMA AUTHORIZATION CURRENT_USER;
         RESET ROLE;",
    )]);
    assert!(db.namespace_oid("acl_cs_user").is_some());
    assert!(db.has_schema_privilege(
        "acl_cs_user",
        namespace(&db, "acl_cs_user"),
        Privilege::Create
    ));

    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_cs_owner;
         CREATE SCHEMA AUTHORIZATION acl_cs_owner;",
    )]);
    assert!(db.namespace_oid("acl_cs_owner").is_some());

    // The migrations' own user is unnamed: the schema's name is unknowable,
    // and the statement is accepted without creating anything (as before).
    let mut db = PgCatalog::new().unwrap();
    db.skip_pg_sanity();
    db.apply_sql("CREATE SCHEMA AUTHORIZATION CURRENT_USER;")
        .unwrap();
    assert!(db.namespace_oid(MIGRATIONS_USER).is_none());
}

#[test]
fn do_block_create_role_if_not_exists_creates_the_role() {
    let db = build_db(&[
        (
            "0001.sql",
            "DO $$ BEGIN
                 IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'acl_do_app') THEN
                     CREATE ROLE acl_do_app LOGIN;
                 END IF;
             END $$;
             DO $$ BEGIN
                 IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'acl_do_user') THEN
                     CREATE USER acl_do_user;
                 END IF;
             END $$;",
        ),
        (
            "0002.sql",
            "DO $$ BEGIN
                 IF NOT EXISTS (SELECT FROM pg_user WHERE usename = 'acl_do_user') THEN
                     CREATE USER acl_do_user;
                 END IF;
             END $$;",
        ),
    ]);
    // The second block found its role and did nothing: a CREATE USER would
    // have failed with "already exists" and failed the migration.
    assert!(db.role_exists("acl_do_app"));
    assert!(db.role_exists("acl_do_user"));
}

#[test]
fn object_grants_require_the_grantor_to_be_the_current_user() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl_grantby_other;
         CREATE TABLE acl_grantby_t (id int);
         GRANT SELECT ON acl_grantby_t TO acl_grantby_other
             GRANTED BY acl_grantby_other;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::UnsupportedDdl(_),
        "grantor must be current user"
    );
    // `GRANTED BY` the current user is accepted.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_grantby_app;
         CREATE TABLE acl_grantby_t2 (id int);
         GRANT SELECT ON acl_grantby_t2 TO acl_grantby_app GRANTED BY postgres;",
    )]);
    assert!(table_has(
        &db,
        "acl_grantby_app",
        "public",
        "acl_grantby_t2",
        Privilege::Select
    ));
}

// ─── Role/ACL model, second round ───────────────────────────────────────────
//
// Reviewer reproducers: `GRANTED BY CURRENT_USER`, table revokes taking the
// column grants along, DROP OWNED's grantor selection and RESTRICT check,
// range-constructor default privileges, DROP ROLE details, and the DO-block
// `CREATE ROLE IF NOT EXISTS` idiom with more than one statement.

#[test]
fn role_grant_granted_by_current_user_is_accepted() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl2_gb_app;
         CREATE ROLE acl2_gb_r;
         CREATE TABLE acl2_gb_t (id int);
         GRANT SELECT ON acl2_gb_t TO acl2_gb_r;
         GRANT acl2_gb_r TO acl2_gb_app GRANTED BY CURRENT_USER;",
    )]);
    assert!(table_has(
        &db,
        "acl2_gb_app",
        "public",
        "acl2_gb_t",
        Privilege::Select
    ));
}

#[test]
fn role_revoke_granted_by_current_user_removes_the_grant() {
    let db = build_db(&[
        (
            "0001.sql",
            "CREATE ROLE acl2_gb2_app;
             CREATE ROLE acl2_gb2_r;
             CREATE TABLE acl2_gb2_t (id int);
             GRANT SELECT ON acl2_gb2_t TO acl2_gb2_r;
             GRANT acl2_gb2_r TO acl2_gb2_app GRANTED BY CURRENT_USER;",
        ),
        (
            "0002.sql",
            "REVOKE acl2_gb2_r FROM acl2_gb2_app GRANTED BY CURRENT_USER;",
        ),
    ]);
    assert!(!table_has(
        &db,
        "acl2_gb2_app",
        "public",
        "acl2_gb2_t",
        Privilege::Select
    ));
}

#[test]
fn drop_owned_by_role_under_set_role_keeps_the_owners_grant() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl2_dro_x;
         CREATE TABLE acl2_dro_t (id int);
         GRANT SELECT ON acl2_dro_t TO acl2_dro_x;
         SET ROLE acl2_dro_x;
         DROP OWNED BY acl2_dro_x;
         RESET ROLE;",
    )]);
    // DROP OWNED runs RemoveRoleFromObjectACL as the current user, who holds
    // no grant options: the owner's grant stays.
    assert!(table_has(
        &db,
        "acl2_dro_x",
        "public",
        "acl2_dro_t",
        Privilege::Select
    ));
}

#[test]
fn drop_owned_restrict_fails_on_dependents_of_types_and_functions() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl2_do_x;
         GRANT CREATE ON SCHEMA public TO acl2_do_x;
         SET ROLE acl2_do_x;
         CREATE TYPE acl2_do_e AS ENUM ('a');
         CREATE FUNCTION acl2_do_f() RETURNS int LANGUAGE sql AS 'select 1';
         RESET ROLE;
         CREATE TABLE acl2_do_t (id acl2_do_e DEFAULT 'a', n int DEFAULT acl2_do_f());
         DROP OWNED BY acl2_do_x;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "cannot drop desired object(s) because other objects depend on them"
    );
}

#[test]
fn drop_owned_cascade_drops_dependents_of_types_and_functions() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl2_doc_x;
         GRANT CREATE ON SCHEMA public TO acl2_doc_x;
         SET ROLE acl2_doc_x;
         CREATE TYPE acl2_doc_e AS ENUM ('a');
         CREATE FUNCTION acl2_doc_f() RETURNS int LANGUAGE sql AS 'select 1';
         RESET ROLE;
         CREATE TABLE acl2_doc_t (id acl2_doc_e DEFAULT 'a', n int DEFAULT acl2_doc_f());
         DROP OWNED BY acl2_doc_x CASCADE;",
    )]);
    assert!(
        db.resolve_type_by_name(Some("public"), "acl2_doc_e")
            .is_none()
    );
    assert!(db.find_functions(Some("public"), "acl2_doc_f").is_empty());
    // The table itself belonged to another role: it survives.
    assert!(db.resolve_table(Some("public"), "acl2_doc_t").is_some());
}

#[test]
fn range_constructors_take_the_migrations_users_default_privileges() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl2_rng_app;
         ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         CREATE TYPE acl2_rng_fr AS RANGE (subtype = float8);",
    )]);
    let constructor = db
        .find_functions(Some("public"), "acl2_rng_fr")
        .into_iter()
        .find(|p| p.proargtypes.len() == 2)
        .expect("range constructor");
    // The constructors are bootstrap-owned, and the migrations' user is the
    // bootstrap superuser: its default privileges apply to them.
    assert!(!db.has_function_privilege("acl2_rng_app", constructor.oid));
    assert!(db.has_function_privilege(MIGRATIONS_USER, constructor.oid));
}

#[test]
fn another_roles_default_privileges_do_not_touch_range_constructors() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl2_rng4_app;
         CREATE ROLE acl2_rng4_x;
         GRANT CREATE ON SCHEMA public TO acl2_rng4_x;
         ALTER DEFAULT PRIVILEGES FOR ROLE acl2_rng4_x
             REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         SET ROLE acl2_rng4_x;
         CREATE TYPE acl2_rng4_fr AS RANGE (subtype = float8);
         RESET ROLE;",
    )]);
    let constructor = db
        .find_functions(Some("public"), "acl2_rng4_fr")
        .into_iter()
        .find(|p| p.proargtypes.len() == 2)
        .expect("range constructor");
    // Constructors stay bootstrap-owned: x's default privileges don't apply.
    assert!(db.has_function_privilege("acl2_rng4_app", constructor.oid));
}

#[test]
fn range_constructor_privileges_can_be_revoked_and_granted() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl2_rng1_app;
         CREATE TYPE acl2_rng1_fr AS RANGE (subtype = float8);
         ALTER TYPE acl2_rng1_fr OWNER TO acl2_rng1_app;
         REVOKE EXECUTE ON FUNCTION acl2_rng1_fr(float8, float8) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION acl2_rng1_fr(float8, float8, text) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION acl2_rng1_fr_multirange(acl2_rng1_fr) FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION acl2_rng1_fr(float8, float8) TO acl2_rng1_app;",
    )]);
    let two = db
        .find_functions(Some("public"), "acl2_rng1_fr")
        .into_iter()
        .find(|p| p.proargtypes.len() == 2)
        .expect("range constructor");
    let three = db
        .find_functions(Some("public"), "acl2_rng1_fr")
        .into_iter()
        .find(|p| p.proargtypes.len() == 3)
        .expect("range constructor with collation");
    let multirange = db
        .find_functions(Some("public"), "acl2_rng1_fr_multirange")
        .into_iter()
        .find(|p| p.proargtypes.len() == 1)
        .expect("multirange constructor");
    assert!(db.has_function_privilege("acl2_rng1_app", two.oid));
    assert!(!db.has_function_privilege("acl2_rng1_app", three.oid));
    assert!(!db.has_function_privilege("acl2_rng1_app", multirange.oid));
}

#[test]
fn drop_role_details_name_column_privileges_in_pg_order() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl2_det_x;
         CREATE TABLE acl2_det_t (id int, b int);
         GRANT SELECT (b) ON acl2_det_t TO acl2_det_x;
         ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO acl2_det_x;
         DROP ROLE acl2_det_x;",
    )]);
    // PG sorts the details by the dependent object's OID — the table was
    // created before the default ACL — and names the column without
    // repeating the table.
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "role \"acl2_det_x\" cannot be dropped because some objects depend on it (privileges \
         for column b of table acl2_det_t; privileges for default privileges on new relations \
         belonging to role postgres)"
    );
}

#[test]
fn do_block_role_existence_branch_runs_other_statements() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl2_do3_app;
         CREATE TABLE acl2_do3_t (id int);
         DO $$ BEGIN
             IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'acl2_do3_r') THEN
                 CREATE ROLE acl2_do3_r;
                 GRANT SELECT ON acl2_do3_t TO acl2_do3_r;
             END IF;
         END $$;
         GRANT acl2_do3_r TO acl2_do3_app;",
    )]);
    assert!(table_has(
        &db,
        "acl2_do3_app",
        "public",
        "acl2_do3_t",
        Privilege::Select
    ));
}

#[test]
fn do_block_role_existence_branch_can_create_a_role_in_role() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE acl2_do4_t (id int);
         DO $$ BEGIN
             IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'acl2_do4_app') THEN
                 CREATE ROLE acl2_do4_app;
             END IF;
         END $$;
         DO $$ BEGIN
             IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'acl2_do4_r') THEN
                 CREATE ROLE acl2_do4_r IN ROLE acl2_do4_app;
             END IF;
         END $$;
         DROP ROLE acl2_do4_r;",
    )]);
    assert!(db.role_exists("acl2_do4_app"));
    assert!(!db.role_exists("acl2_do4_r"));
}

#[test]
fn do_block_skips_the_then_branch_when_the_role_exists() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl2_do2_app;
         CREATE TABLE acl2_do2_t (id int);
         DO $$ BEGIN
             IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles
                            WHERE rolname = 'acl2_do2_app') THEN
                 CREATE ROLE acl2_do2_app SUPERUSER;
             END IF;
         END $$;",
    )]);
    // The role existed, so it did not become a superuser: it still cannot
    // read the table.
    assert!(
        db.analyze_as("SELECT id FROM acl2_do2_t", "acl2_do2_app")
            .is_err()
    );
}

#[test]
fn reassign_owned_by_current_user_refuses_the_bootstrap_superuser() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE ROLE acl2_reas_app;
         CREATE TABLE acl2_reas_t (id int);
         REASSIGN OWNED BY CURRENT_USER TO acl2_reas_app;",
    )]);
    // The migrations' user is the bootstrap superuser; PG names it.
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "cannot reassign ownership of objects owned by role postgres because they are required \
         by the database system"
    );
}

#[test]
fn drop_owned_by_current_user_refuses_the_bootstrap_superuser() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TABLE acl2_dropcu_t (id int);
         DROP OWNED BY CURRENT_USER;",
    )]);
    assert_err_prefix!(
        result,
        DdlError::DependencyError(_),
        "cannot drop objects owned by role postgres because they are required by the database \
         system"
    );
}

#[test]
fn created_roles_keep_their_attributes() {
    // The oracle compares every created role's pg_authid attributes with the
    // server's; exercise the options the model records.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl2_attr_a CREATEROLE CREATEDB LOGIN INHERIT;
         CREATE ROLE acl2_attr_b NOLOGIN NOINHERIT;
         ALTER ROLE acl2_attr_a NOCREATEDB;
         ALTER ROLE acl2_attr_b CREATEROLE;",
    )]);
    assert!(db.role_exists("acl2_attr_a"));
    assert!(db.role_exists("acl2_attr_b"));
}

// ─── Role/ACL model, third round: DROP OWNED's explicit ACLs ───────────────

#[test]
fn drop_owned_keeps_an_explicit_empty_acl() {
    // RemoveRoleFromObjectACL leaves an explicit empty ACL: the object must
    // not fall back to its default ACL (which grants PUBLIC EXECUTE).
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_t7_empty_removed;
         CREATE ROLE acl_t7_empty_app;
         CREATE FUNCTION acl_t7_empty_f() RETURNS int LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         REVOKE EXECUTE ON FUNCTION acl_t7_empty_f() FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION acl_t7_empty_f() FROM CURRENT_USER;
         GRANT EXECUTE ON FUNCTION acl_t7_empty_f() TO acl_t7_empty_removed;
         DROP OWNED BY acl_t7_empty_removed;",
    )]);
    assert!(!db.has_function_privilege(
        "acl_t7_empty_app",
        proc_oid(&db, "public", "acl_t7_empty_f")
    ));
}

#[test]
fn drop_owned_reaches_column_acls_without_a_table_acl() {
    // shdepDropOwned reaches column grants through the role's shared
    // dependencies even when the relation itself has no explicit ACL.
    let db = build_db(&[(
        "0001.sql",
        "CREATE ROLE acl_t7_coldrop_app;
         CREATE TABLE acl_t7_coldrop_t (a int, b int);
         GRANT SELECT (a) ON acl_t7_coldrop_t TO acl_t7_coldrop_app;
         DROP OWNED BY acl_t7_coldrop_app;",
    )]);
    let table = relation(&db, "public", "acl_t7_coldrop_t");
    let a = attnum(&db, table, "a");
    assert!(!db.has_column_privilege("acl_t7_coldrop_app", table, a, Privilege::Select));
    assert!(!db.has_any_column_privilege("acl_t7_coldrop_app", table, Privilege::Select));
}
