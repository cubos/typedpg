//! PG 18 not-null constraints: named, NOT VALID, NO INHERIT, inherited;
//! what `pg_constraint` / `attnotnull` record, the errors PG raises, and
//! the nullability queries see. Expected messages are PG 18's.

use crate::common::*;
use typedpg_analyzer::ConType;

fn ddl_err(setup: &str, stmt: &str) -> String {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(setup).unwrap();
    match db.apply_sql(stmt) {
        Ok(()) => panic!("expected {stmt:?} to fail"),
        Err(e) => e.to_string(),
    }
}

fn assert_err(setup: &str, stmt: &str, expected_prefix: &str) {
    let msg = ddl_err(setup, stmt);
    assert!(
        msg.starts_with(expected_prefix),
        "{stmt}\n  expected prefix: {expected_prefix:?}\n  got:             {msg:?}"
    );
}

/// `(conname, convalidated, connoinherit, conislocal, coninhcount)` of the
/// table's not-null constraints.
fn not_nulls(db: &PgCatalog, table: &str) -> Vec<(String, bool, bool, bool, i16)> {
    db.constraints_of_table(table)
        .into_iter()
        .filter(|c| c.contype == ConType::NotNull)
        .map(|c| {
            (
                c.conname,
                c.convalidated,
                c.connoinherit,
                c.conislocal,
                c.coninhcount,
            )
        })
        .collect()
}

fn attnotnull(db: &PgCatalog, table: &str, column: &str) -> bool {
    let t = db.resolve_table(None, table).unwrap();
    db.attributes_of(t.oid)
        .iter()
        .find(|a| a.attname == column)
        .unwrap()
        .attnotnull
}

fn nn(
    name: &str,
    valid: bool,
    no_inherit: bool,
    local: bool,
    inh: i16,
) -> (String, bool, bool, bool, i16) {
    (name.to_owned(), valid, no_inherit, local, inh)
}

#[test]
fn not_valid_not_null_sets_attnotnull_but_queries_see_nulls() {
    // PG 18: attnotnull = t even while NOT VALID, yet rows that predate the
    // constraint may be NULL (the planner only trusts validated ones).
    let mut db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int, c int);
         ALTER TABLE t ADD CONSTRAINT a_nn NOT NULL a NOT VALID;",
    )]);
    assert_eq!(not_nulls(&db, "t"), vec![nn("a_nn", false, false, true, 0)]);
    assert!(attnotnull(&db, "t", "a"));
    let q = db.analyze("SELECT a FROM t").unwrap();
    assert!(
        q.columns[0].nullable,
        "a NOT VALID not-null column may be NULL"
    );

    db.apply_sql("ALTER TABLE t VALIDATE CONSTRAINT a_nn;")
        .unwrap();
    assert_eq!(not_nulls(&db, "t"), vec![nn("a_nn", true, false, true, 0)]);
    let q = db.analyze("SELECT a FROM t").unwrap();
    assert!(!q.columns[0].nullable);

    // SET NOT NULL validates a NOT VALID one.
    db.apply_sql(
        "ALTER TABLE t ADD CONSTRAINT b_nn NOT NULL b NOT VALID;
         ALTER TABLE t ALTER COLUMN b SET NOT NULL;",
    )
    .unwrap();
    assert_eq!(not_nulls(&db, "t")[1], nn("b_nn", true, false, true, 0));
    // Inserting NULL is refused either way: the constraint is enforced.
    db.apply_sql("ALTER TABLE t ADD CONSTRAINT c_nn NOT NULL c NOT VALID;")
        .unwrap();
    let err = db
        .analyze("INSERT INTO t (a, b, c) VALUES (1, 1, NULL)")
        .unwrap_err();
    assert!(
        err.to_string().starts_with(
            "null value in column \"c\" of relation \"t\" violates not-null constraint"
        ),
        "{err}"
    );
}

#[test]
fn named_not_null_constraints_in_create_table() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t2 (a int CONSTRAINT a2_nn NOT NULL NO INHERIT, b int,
            CONSTRAINT b2_nn NOT NULL b);
         CREATE TABLE t3 (a int, CONSTRAINT a3_nn NOT NULL a NOT VALID);
         CREATE TABLE t7 (a int, NOT NULL a);
         CREATE TABLE t10 (a int NOT NULL, CONSTRAINT n2 NOT NULL a);
         CREATE TABLE t11 (a int, b int, NOT NULL a, NOT NULL a);",
    )]);
    assert_eq!(
        not_nulls(&db, "t2"),
        vec![
            nn("a2_nn", true, true, true, 0),
            nn("b2_nn", true, false, true, 0)
        ]
    );
    // CREATE TABLE's not-null constraints are valid: the table is empty.
    assert_eq!(
        not_nulls(&db, "t3"),
        vec![nn("a3_nn", true, false, true, 0)]
    );
    assert_eq!(
        not_nulls(&db, "t7"),
        vec![nn("t7_a_not_null", true, false, true, 0)]
    );
    assert_eq!(not_nulls(&db, "t10"), vec![nn("n2", true, false, true, 0)]);
    assert_eq!(
        not_nulls(&db, "t11"),
        vec![nn("t11_a_not_null", true, false, true, 0)]
    );

    for (stmt, msg) in [
        (
            "CREATE TABLE t (a int CONSTRAINT n1 NOT NULL CONSTRAINT n2 NOT NULL);",
            "conflicting not-null constraint names \"n1\" and \"n2\"",
        ),
        (
            "CREATE TABLE t (a int CONSTRAINT n1 NOT NULL, CONSTRAINT n2 NOT NULL a);",
            "conflicting not-null constraint names \"n1\" and \"n2\"",
        ),
        (
            "CREATE TABLE t (a int NOT NULL, NOT NULL a NO INHERIT);",
            "conflicting NO INHERIT declaration for not-null constraint on column \"a\"",
        ),
        (
            "CREATE TABLE t (a int PRIMARY KEY NOT NULL NO INHERIT);",
            "conflicting NO INHERIT declarations for not-null constraints on column \"a\"",
        ),
        (
            "CREATE TABLE t (a int, b int, CONSTRAINT dup NOT NULL a, CONSTRAINT dup NOT NULL b);",
            "constraint \"dup\" for relation \"t\" already exists",
        ),
        (
            "CREATE TABLE t (a int NOT NULL DEFERRABLE);",
            "misplaced DEFERRABLE clause",
        ),
        (
            "CREATE TABLE t (a int NOT NULL NOT ENFORCED);",
            "misplaced NOT ENFORCED clause",
        ),
        (
            "CREATE TABLE t (a int NOT NULL NO INHERIT) PARTITION BY LIST (a);",
            "not-null constraints on partitioned tables cannot be NO INHERIT",
        ),
    ] {
        assert_err("", stmt, msg);
    }
}

#[test]
fn not_null_inheritance_in_create_table() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int);
         ALTER TABLE p ADD CONSTRAINT pa_nn NOT NULL a NOT VALID;
         CREATE TABLE c () INHERITS (p);
         CREATE TABLE t15 (a int NOT NULL NO INHERIT);
         CREATE TABLE t15c () INHERITS (t15);",
    )]);
    // A child created from a NOT VALID parent constraint gets a valid one.
    assert_eq!(
        not_nulls(&db, "c"),
        vec![nn("pa_nn", true, false, false, 1)]
    );
    assert!(attnotnull(&db, "c", "a"));
    // A NO INHERIT constraint stays with its table.
    assert!(not_nulls(&db, "t15c").is_empty());
    assert!(!attnotnull(&db, "t15c", "a"));
    assert_err(
        "CREATE TABLE ncp (a int NOT NULL);",
        "CREATE TABLE ncc (a int NOT NULL NO INHERIT) INHERITS (ncp);",
        "cannot define not-null constraint with NO INHERIT on column \"a\"",
    );
}

#[test]
fn add_constraint_not_null_merges_with_the_existing_constraint() {
    let setup = "CREATE TABLE t (a int, b int, c int);
        ALTER TABLE t ADD CONSTRAINT a_nn NOT NULL a NOT VALID;
        ALTER TABLE t ADD CONSTRAINT b_nn NOT NULL b;
        ALTER TABLE t ADD CONSTRAINT c_nn NOT NULL c NO INHERIT;";
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE t ADD CONSTRAINT b_nn NOT NULL b;
             ALTER TABLE t ADD NOT NULL b;",
        ),
    ]);
    assert_eq!(
        not_nulls(&db, "t"),
        vec![
            nn("a_nn", false, false, true, 0),
            nn("b_nn", true, false, true, 0),
            nn("c_nn", true, true, true, 0),
        ]
    );
    for (stmt, msg) in [
        (
            "ALTER TABLE t ADD CONSTRAINT b_nn2 NOT NULL b;",
            "cannot create not-null constraint \"b_nn2\" on column \"b\" of table \"t\"",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT a_nn2 NOT NULL a;",
            "incompatible NOT VALID constraint \"a_nn\" on relation \"t\"",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT c_nn NOT NULL c;",
            "cannot change NO INHERIT status of NOT NULL constraint \"c_nn\" on relation \"t\"",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT x NOT NULL nosuch;",
            "column \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT x NOT NULL xmin;",
            "cannot add not-null constraint on system column \"xmin\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN c SET NOT NULL;",
            "cannot change NO INHERIT status of NOT NULL constraint \"c_nn\" on relation \"t\"",
        ),
        (
            "ALTER TABLE t ALTER COLUMN xmin SET NOT NULL;",
            "cannot alter system column \"xmin\"",
        ),
        (
            "ALTER TABLE t ALTER CONSTRAINT a_nn DEFERRABLE;",
            "constraint \"a_nn\" of relation \"t\" is not a foreign key constraint",
        ),
        (
            "ALTER TABLE t ALTER CONSTRAINT a_nn NOT ENFORCED;",
            "cannot alter enforceability of constraint \"a_nn\" of relation \"t\"",
        ),
    ] {
        assert_err(setup, stmt, msg);
    }
}

#[test]
fn not_null_constraints_reach_the_children() {
    let setup = "CREATE TABLE p (a int, b int); CREATE TABLE c () INHERITS (p);";
    assert_err(
        setup,
        "ALTER TABLE ONLY p ADD CONSTRAINT nn NOT NULL a;",
        "constraint must be added to child tables too",
    );
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE ONLY p ALTER COLUMN a SET NOT NULL;
             ALTER TABLE p ADD CONSTRAINT vnn NOT NULL b NOT VALID;",
        ),
    ]);
    // ONLY on a regular parent makes the constraint NO INHERIT.
    assert_eq!(
        not_nulls(&db, "p"),
        vec![
            nn("p_a_not_null", true, true, true, 0),
            nn("vnn", false, false, true, 0)
        ]
    );
    assert_eq!(not_nulls(&db, "c"), vec![nn("vnn", false, false, false, 1)]);
    assert_err(
        &format!("{setup} ALTER TABLE p ADD CONSTRAINT vnn NOT NULL b NOT VALID;"),
        "ALTER TABLE ONLY p VALIDATE CONSTRAINT vnn;",
        "constraint must be validated on child tables too",
    );
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE p ADD CONSTRAINT vnn NOT NULL b NOT VALID;
             ALTER TABLE p VALIDATE CONSTRAINT vnn;",
        ),
    ]);
    assert_eq!(not_nulls(&db, "c"), vec![nn("vnn", true, false, false, 1)]);
    assert_err(
        "CREATE TABLE pp (a int) PARTITION BY LIST (a);
         CREATE TABLE pp1 PARTITION OF pp FOR VALUES IN (1);",
        "ALTER TABLE ONLY pp ALTER COLUMN a SET NOT NULL;",
        "constraint must be added to child tables too",
    );
}

#[test]
fn alter_constraint_changes_not_null_inheritability() {
    let setup = "CREATE TABLE nip (a int NOT NULL); CREATE TABLE nic () INHERITS (nip);";
    assert_err(
        setup,
        "ALTER TABLE nic ALTER CONSTRAINT nip_a_not_null NO INHERIT;",
        "cannot alter inherited constraint \"nip_a_not_null\" on relation \"nic\"",
    );
    let mut db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER TABLE nip ALTER CONSTRAINT nip_a_not_null NO INHERIT;",
        ),
    ]);
    assert_eq!(
        not_nulls(&db, "nip"),
        vec![nn("nip_a_not_null", true, true, true, 0)]
    );
    assert_eq!(
        not_nulls(&db, "nic"),
        vec![nn("nip_a_not_null", true, false, true, 0)]
    );
    db.apply_sql("ALTER TABLE nip ALTER CONSTRAINT nip_a_not_null INHERIT;")
        .unwrap();
    assert_eq!(
        not_nulls(&db, "nip"),
        vec![nn("nip_a_not_null", true, false, true, 0)]
    );
    assert_eq!(
        not_nulls(&db, "nic"),
        vec![nn("nip_a_not_null", true, false, true, 1)]
    );
    assert_err(
        "CREATE TABLE pp (a int NOT NULL) PARTITION BY LIST (a);",
        "ALTER TABLE pp ALTER CONSTRAINT pp_a_not_null NO INHERIT;",
        "not-null constraint \"pp_a_not_null\" on partitioned table \"pp\" cannot be NO INHERIT",
    );
    assert_err(
        "CREATE TABLE t (a int CHECK (a > 0));",
        "ALTER TABLE t ALTER CONSTRAINT t_a_check NO INHERIT;",
        "constraint \"t_a_check\" of relation \"t\" is not a not-null constraint",
    );
}

#[test]
fn primary_keys_and_identities_need_a_valid_inheritable_not_null() {
    assert_err(
        "CREATE TABLE q4 (a int); ALTER TABLE q4 ADD CONSTRAINT q4_nn NOT NULL a NOT VALID;",
        "ALTER TABLE q4 ADD PRIMARY KEY (a);",
        "cannot create primary key on column \"a\"",
    );
    assert_err(
        "CREATE TABLE ni (a int NOT NULL NO INHERIT);",
        "ALTER TABLE ni ADD PRIMARY KEY (a);",
        "cannot create primary key on column \"a\"",
    );
    assert_err(
        "CREATE TABLE q5 (a int); ALTER TABLE q5 ADD CONSTRAINT q5_nn NOT NULL a NOT VALID;",
        "ALTER TABLE q5 ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY;",
        "incompatible NOT VALID constraint \"q5_nn\" on relation \"q5\"",
    );
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE q3 (a int);
         ALTER TABLE q3 ADD CONSTRAINT q3_nn NOT NULL a NOT VALID;
         ALTER TABLE q3 ALTER COLUMN a SET NOT NULL;
         ALTER TABLE q3 ADD PRIMARY KEY (a);
         CREATE TABLE pk (a int);
         CREATE UNIQUE INDEX pk_idx ON pk (a);
         ALTER TABLE pk ADD PRIMARY KEY USING INDEX pk_idx;",
    )]);
    assert_eq!(
        not_nulls(&db, "q3"),
        vec![nn("q3_nn", true, false, true, 0)]
    );
    assert_eq!(
        not_nulls(&db, "pk"),
        vec![nn("pk_a_not_null", true, false, true, 0)]
    );
}

#[test]
fn dropping_not_null_constraints() {
    // Audit #125: the constraint can be dropped by name.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (id int NOT NULL);
         ALTER TABLE t DROP CONSTRAINT t_id_not_null;
         CREATE TABLE t13 (id int CONSTRAINT id_nn NOT NULL);
         ALTER TABLE t13 DROP CONSTRAINT id_nn;",
    )]);
    assert!(!attnotnull(&db, "t", "id"));
    assert!(!attnotnull(&db, "t13", "id"));
    assert_err(
        "CREATE TABLE t14 (a int PRIMARY KEY);",
        "ALTER TABLE t14 DROP CONSTRAINT t14_a_not_null;",
        "column \"a\" is in a primary key",
    );
    let ri = "CREATE TABLE ri (a int NOT NULL);
        CREATE UNIQUE INDEX ri_idx ON ri (a);
        ALTER TABLE ri REPLICA IDENTITY USING INDEX ri_idx;";
    assert_err(
        ri,
        "ALTER TABLE ri ALTER COLUMN a DROP NOT NULL;",
        "column \"a\" is in index used as replica identity",
    );
    assert_err(
        ri,
        "ALTER TABLE ri DROP CONSTRAINT ri_a_not_null;",
        "column \"a\" is in index used as replica identity",
    );
    assert_err(
        "CREATE TABLE idt (a int GENERATED ALWAYS AS IDENTITY);",
        "ALTER TABLE idt DROP CONSTRAINT idt_a_not_null;",
        "column \"a\" of relation \"idt\" is an identity column",
    );
    build_db(&[(
        "0001.sql",
        &format!(
            "{ri} ALTER TABLE ri REPLICA IDENTITY FULL; ALTER TABLE ri ALTER COLUMN a DROP NOT NULL;"
        ),
    )]);
}

#[test]
fn alter_inherit_matches_not_null_constraints() {
    let setup = "CREATE TABLE ip (a int NOT NULL);
        CREATE TABLE ic (a int); ALTER TABLE ic ADD CONSTRAINT icnn NOT NULL a NO INHERIT;
        CREATE TABLE ic2 (a int); ALTER TABLE ic2 ADD CONSTRAINT ic2nn NOT NULL a NOT VALID;";
    assert_err(
        setup,
        "ALTER TABLE ic INHERIT ip;",
        "constraint \"icnn\" conflicts with non-inherited constraint on child table \"ic\"",
    );
    assert_err(
        setup,
        "ALTER TABLE ic2 INHERIT ip;",
        "constraint \"ic2nn\" conflicts with NOT VALID constraint on child table \"ic2\"",
    );
    // A NO INHERIT parent constraint asks nothing of the child.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE ip3 (a int CONSTRAINT ip3nn NOT NULL NO INHERIT);
         CREATE TABLE ic3 (a int);
         ALTER TABLE ic3 INHERIT ip3;",
    )]);
}

#[test]
fn like_copies_not_null_constraints_with_their_names() {
    // transformTableLikeClause copies the source's not-null constraints,
    // names and NO INHERIT flags included.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE s (a int NOT NULL, b int CONSTRAINT bnn NOT NULL NO INHERIT);
         CREATE TABLE t (LIKE s);
         ALTER TABLE t RENAME CONSTRAINT s_a_not_null TO a_nn;",
    )]);
    assert_eq!(
        not_nulls(&db, "t"),
        vec![
            nn("a_nn", true, false, true, 0),
            nn("bnn", true, true, true, 0)
        ]
    );
    assert_err(
        "CREATE TABLE s (a int CONSTRAINT foo NOT NULL);",
        "CREATE TABLE t (LIKE s, CONSTRAINT foo2 NOT NULL a);",
        "conflicting not-null constraint names \"foo\" and \"foo2\"",
    );
}

#[test]
fn partitioned_tables_take_no_no_inherit_not_null() {
    let msg = "not-null constraints on partitioned tables cannot be NO INHERIT";
    assert_err(
        "",
        "CREATE TABLE p (a int, NOT NULL a NO INHERIT) PARTITION BY LIST (a);",
        msg,
    );
    assert_err(
        "CREATE TABLE p (a int) PARTITION BY LIST (a);",
        "ALTER TABLE p ADD CONSTRAINT nn NOT NULL a NO INHERIT;",
        msg,
    );
}

#[test]
fn constraint_names_collide_in_create_table_like_pg() {
    // DefineRelation (PG 18) creates the CHECK constraints first, then the
    // not-null ones, then the index-backed ones. An auto-generated not-null
    // name skips the names taken; a given one isn't checked against the
    // CHECK constraints, so a clash fails on pg_constraint's unique index.
    let dup_key = "duplicate key value violates unique constraint \"pg_constraint_conrelid_contypid_conname_index\"";
    for (stmt, msg) in [
        (
            "CREATE TABLE z3 (a int check (a > 0), constraint z3_a_check not null a);",
            dup_key,
        ),
        (
            "CREATE TABLE z7 (a int constraint c7 not null, constraint c7 check (a > 0));",
            dup_key,
        ),
        (
            "CREATE TABLE z12 (a int not null, constraint z12_a_not_null check (a > 0), \
             b int constraint z12_a_not_null1 not null);",
            dup_key,
        ),
        (
            "CREATE TABLE z14 (a int constraint z14_a_check not null check (a > 0));",
            dup_key,
        ),
        (
            "CREATE TABLE z6 (a int, constraint c1 check (a > 0), constraint c1 unique (a));",
            "constraint \"c1\" for relation \"z6\" already exists",
        ),
        (
            "CREATE TABLE z16 (a int, constraint c1 unique (a), constraint c1 check (a > 0));",
            "constraint \"c1\" for relation \"z16\" already exists",
        ),
        (
            "CREATE TABLE z15 (a int, b int check (b > 0), constraint z15_b_check unique (a));",
            "constraint \"z15_b_check\" for relation \"z15\" already exists",
        ),
        (
            "CREATE TABLE z8 (a int, constraint z8_a_check check (a > 0), \
             constraint z8_a_check check (a > 1));",
            "check constraint \"z8_a_check\" already exists",
        ),
    ] {
        let err = try_apply(&[("0001.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE z4 (a int constraint z4_a_not_null check (a > 0) not null);
         CREATE TABLE z5 (a int not null, constraint z5_a_not_null check (a > 0));
         CREATE TABLE z13 (a int check (a > 0) not null, b int, \
                           constraint z13_a_not_null check (b > 0));
         CREATE TABLE z17 (a int primary key constraint z17_pkey check (a > 0));",
    )]);
    let names = |table: &str| -> Vec<(String, ConType)> {
        db.constraints_of_table(table)
            .into_iter()
            .map(|c| (c.conname, c.contype))
            .collect()
    };
    assert_eq!(
        names("z4"),
        vec![
            ("z4_a_not_null".to_owned(), ConType::Check),
            ("z4_a_not_null1".to_owned(), ConType::NotNull)
        ]
    );
    assert_eq!(
        names("z5"),
        vec![
            ("z5_a_not_null".to_owned(), ConType::Check),
            ("z5_a_not_null1".to_owned(), ConType::NotNull)
        ]
    );
    assert_eq!(
        names("z13"),
        vec![
            ("z13_a_check".to_owned(), ConType::Check),
            ("z13_a_not_null".to_owned(), ConType::Check),
            ("z13_a_not_null1".to_owned(), ConType::NotNull)
        ]
    );
    // The primary key's generated name skips the CHECK constraint's too.
    assert_eq!(
        names("z17"),
        vec![
            ("z17_pkey".to_owned(), ConType::Check),
            ("z17_a_not_null".to_owned(), ConType::NotNull),
            ("z17_pkey1".to_owned(), ConType::PrimaryKey)
        ]
    );
}
