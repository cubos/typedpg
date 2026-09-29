//! PG 18's `[NOT] ENFORCED` CHECK and FOREIGN KEY constraints: what
//! `pg_constraint` records, which constraints refuse the clause, and how
//! enforceability meets validation, ALTER CONSTRAINT and inheritance.
//! Expected messages are PG 18's.

use crate::common::*;
use typedpg_analyzer::ConType;

fn assert_err(setup: &str, stmt: &str, expected_prefix: &str) {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(setup).unwrap();
    let msg = match db.apply_sql(stmt) {
        Ok(()) => panic!("expected {stmt:?} to fail"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.starts_with(expected_prefix),
        "{stmt}\n  expected prefix: {expected_prefix:?}\n  got:             {msg:?}"
    );
}

/// `(conname, contype, conenforced, convalidated)`, by name.
fn flags(db: &PgCatalog, table: &str) -> Vec<(String, ConType, bool, bool)> {
    let mut rows: Vec<_> = db
        .constraints_of_table(table)
        .into_iter()
        .filter(|c| matches!(c.contype, ConType::Check | ConType::ForeignKey))
        .map(|c| (c.conname, c.contype, c.conenforced, c.convalidated))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

fn row(name: &str, contype: ConType, enforced: bool, valid: bool) -> (String, ConType, bool, bool) {
    (name.to_owned(), contype, enforced, valid)
}

const PK: &str = "CREATE TABLE pk (id int PRIMARY KEY);";

#[test]
fn not_enforced_constraints_are_recorded_as_not_valid() {
    let db = build_db(&[
        ("0001.sql", PK),
        (
            "0002.sql",
            "CREATE TABLE fk (a int REFERENCES pk NOT ENFORCED,
                b int CHECK (b > 0) NOT ENFORCED,
                c int CHECK (c > 0) ENFORCED,
                d int REFERENCES pk ENFORCED);
             CREATE TABLE fk2 (a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES pk NOT ENFORCED,
                CONSTRAINT ck CHECK (a > 0) NOT ENFORCED);
             ALTER TABLE fk2 ADD CONSTRAINT f2 FOREIGN KEY (a) REFERENCES pk NOT ENFORCED;
             ALTER TABLE fk2 ADD CONSTRAINT ck2 CHECK (a > 1) NOT ENFORCED;
             ALTER TABLE fk2 ADD CONSTRAINT f3 FOREIGN KEY (a) REFERENCES pk NOT VALID;
             ALTER TABLE fk2 ADD CONSTRAINT ck3 CHECK (a > 2) NOT VALID;
             CREATE TABLE e5 (a int, CHECK (a > 0) NOT ENFORCED NOT VALID);
             CREATE TABLE e6 (a int, CHECK (a > 0) NOT VALID);",
        ),
    ]);
    use ConType::{Check, ForeignKey};
    assert_eq!(
        flags(&db, "fk"),
        vec![
            row("fk_a_fkey", ForeignKey, false, false),
            row("fk_b_check", Check, false, false),
            row("fk_c_check", Check, true, true),
            row("fk_d_fkey", ForeignKey, true, true),
        ]
    );
    assert_eq!(
        flags(&db, "fk2"),
        vec![
            row("ck", Check, false, false),
            row("ck2", Check, false, false),
            row("ck3", Check, true, false),
            row("f", ForeignKey, false, false),
            row("f2", ForeignKey, false, false),
            row("f3", ForeignKey, true, false),
        ]
    );
    assert_eq!(
        flags(&db, "e5"),
        vec![row("e5_a_check", Check, false, false)]
    );
    // CREATE TABLE's constraints are valid: the table is empty.
    assert_eq!(flags(&db, "e6"), vec![row("e6_a_check", Check, true, true)]);
}

#[test]
fn only_check_and_foreign_key_constraints_take_enforceability() {
    for (stmt, msg) in [
        (
            "CREATE TABLE u (a int UNIQUE NOT ENFORCED);",
            "misplaced NOT ENFORCED clause",
        ),
        (
            "CREATE TABLE u (a int PRIMARY KEY NOT ENFORCED);",
            "misplaced NOT ENFORCED clause",
        ),
        (
            "CREATE TABLE u (a int NOT NULL ENFORCED);",
            "misplaced ENFORCED clause",
        ),
        (
            "CREATE TABLE u (a int, UNIQUE (a) NOT ENFORCED);",
            "UNIQUE constraints cannot be marked NOT ENFORCED",
        ),
        (
            "CREATE TABLE u (a int, PRIMARY KEY (a) NOT ENFORCED);",
            "PRIMARY KEY constraints cannot be marked NOT ENFORCED",
        ),
        (
            "CREATE TABLE u (a int, EXCLUDE (a WITH =) NOT ENFORCED);",
            "EXCLUDE constraints cannot be marked NOT ENFORCED",
        ),
        (
            "CREATE TABLE u (a int, NOT NULL a NOT ENFORCED);",
            "NOT NULL constraints cannot be marked NOT ENFORCED",
        ),
        (
            "CREATE TABLE u (a int, UNIQUE (a) ENFORCED);",
            "UNIQUE constraints cannot be marked ENFORCED",
        ),
        (
            "CREATE TABLE u (a int CHECK (a > 0) NOT ENFORCED NOT ENFORCED);",
            "multiple ENFORCED/NOT ENFORCED clauses not allowed",
        ),
        (
            "CREATE TABLE u (a int CHECK (a > 0) ENFORCED NOT ENFORCED);",
            "multiple ENFORCED/NOT ENFORCED clauses not allowed",
        ),
        (
            "CREATE TABLE u (a int DEFAULT 1 NOT ENFORCED);",
            "misplaced NOT ENFORCED clause",
        ),
        (
            "CREATE DOMAIN dn AS int CHECK (VALUE > 0) NOT ENFORCED;",
            "specifying constraint enforceability not supported for domains",
        ),
        (
            "CREATE DOMAIN dn AS int NOT NULL NOT NULL;",
            "redundant NOT NULL constraint definition",
        ),
        (
            "CREATE DOMAIN dn AS int NOT NULL NULL;",
            "conflicting NULL/NOT NULL constraints",
        ),
        (
            "CREATE DOMAIN dn AS int DEFAULT 1 DEFAULT 2;",
            "multiple default expressions",
        ),
    ] {
        assert_err(PK, stmt, msg);
    }
    assert_err(
        "CREATE DOMAIN dn AS int;",
        "ALTER DOMAIN dn ADD CONSTRAINT x CHECK (VALUE > 1) NOT ENFORCED;",
        "CHECK constraints cannot be marked NOT ENFORCED",
    );
    build_db(&[
        ("0001.sql", PK),
        (
            "0002.sql",
            "CREATE TABLE e3 (a int REFERENCES pk NOT ENFORCED DEFERRABLE);",
        ),
    ]);
}

#[test]
fn alter_constraint_changes_foreign_key_enforceability() {
    let setup = format!(
        "{PK} CREATE TABLE fk2 (a int, CONSTRAINT f FOREIGN KEY (a) REFERENCES pk NOT ENFORCED,
            CONSTRAINT ck CHECK (a > 0) NOT ENFORCED);"
    );
    let mut db = build_db(&[(
        "0001.sql",
        &format!("{setup} ALTER TABLE fk2 ALTER CONSTRAINT f ENFORCED;"),
    )]);
    let f = |db: &PgCatalog| flags(db, "fk2").into_iter().find(|r| r.0 == "f").unwrap();
    // Made ENFORCED, it is validated.
    assert_eq!(f(&db), row("f", ConType::ForeignKey, true, true));
    db.apply_sql("ALTER TABLE fk2 ALTER CONSTRAINT f NOT ENFORCED NOT DEFERRABLE;")
        .unwrap();
    assert_eq!(f(&db), row("f", ConType::ForeignKey, false, false));
    assert_err(
        &setup,
        "ALTER TABLE fk2 ALTER CONSTRAINT ck ENFORCED;",
        "cannot alter enforceability of constraint \"ck\" of relation \"fk2\"",
    );
    for name in ["f", "ck"] {
        assert_err(
            &setup,
            &format!("ALTER TABLE fk2 VALIDATE CONSTRAINT {name};"),
            "cannot validate NOT ENFORCED constraint",
        );
    }
}

#[test]
fn inherited_check_constraints_merge_by_enforceability() {
    let setup = "CREATE TABLE cp (a int, CONSTRAINT cc CHECK (a > 0));
        CREATE TABLE cp2 (a int, CONSTRAINT cc CHECK (a > 0) NOT ENFORCED);";
    assert_err(
        &format!("{setup} CREATE TABLE cc1 (a int, CONSTRAINT cc CHECK (a > 0) NOT ENFORCED);"),
        "ALTER TABLE cc1 INHERIT cp;",
        "constraint \"cc\" conflicts with NOT ENFORCED constraint on child table \"cc1\"",
    );
    assert_err(
        setup,
        "CREATE TABLE cc3 (a int, CONSTRAINT cc CHECK (a > 0) NOT ENFORCED) INHERITS (cp);",
        "constraint \"cc\" conflicts with NOT ENFORCED constraint on relation \"cc3\"",
    );
    let db = build_db(&[(
        "0001.sql",
        &format!(
            "{setup}
             CREATE TABLE cc2 (a int, CONSTRAINT cc CHECK (a > 0));
             ALTER TABLE cc2 INHERIT cp2;
             CREATE TABLE cc4 (a int, CONSTRAINT cc CHECK (a > 0)) INHERITS (cp2);
             CREATE TABLE cc5 () INHERITS (cp2);
             CREATE TABLE cc6 () INHERITS (cp);
             ALTER TABLE cp ADD CONSTRAINT cc2 CHECK (a > 1) NOT ENFORCED;"
        ),
    )]);
    assert_eq!(
        flags(&db, "cc4"),
        vec![row("cc", ConType::Check, true, true)]
    );
    // A child inherits the parent's NOT ENFORCED constraint as such.
    assert_eq!(
        flags(&db, "cc5"),
        vec![row("cc", ConType::Check, false, false)]
    );
    assert_eq!(
        flags(&db, "cc6"),
        vec![
            row("cc", ConType::Check, true, true),
            row("cc2", ConType::Check, false, false),
        ]
    );
}
