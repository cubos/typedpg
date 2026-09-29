//! PG 18 temporal constraints: PRIMARY KEY / UNIQUE `(..., col WITHOUT
//! OVERLAPS)` and FOREIGN KEY `(..., PERIOD col) REFERENCES t (...,
//! PERIOD col)`. Expected messages are PG 18's.

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

const T: &str = "CREATE TABLE t (id int4range, valid_at daterange,
    CONSTRAINT t_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));";

#[test]
fn without_overlaps_keys_are_recorded_as_temporal() {
    let db = build_db(&[(
        "0001.sql",
        &format!(
            "{T}
             CREATE TABLE t_u (id int4range, valid_at daterange,
                UNIQUE (id, valid_at WITHOUT OVERLAPS));
             CREATE TABLE t_mr (id int4range, valid_at datemultirange,
                PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
             CREATE TABLE t_inc (id int4range, valid_at daterange, x int,
                PRIMARY KEY (id, valid_at WITHOUT OVERLAPS) INCLUDE (x));
             CREATE TABLE t_nov (id int4range, valid_at daterange);
             ALTER TABLE t_nov ADD PRIMARY KEY (id, valid_at WITHOUT OVERLAPS);"
        ),
    )]);
    for (table, contype) in [
        ("t", ConType::PrimaryKey),
        ("t_u", ConType::Unique),
        ("t_mr", ConType::PrimaryKey),
        ("t_inc", ConType::PrimaryKey),
        ("t_nov", ConType::PrimaryKey),
    ] {
        let key = db
            .constraints_of_table(table)
            .into_iter()
            .find(|c| c.contype == contype)
            .unwrap();
        assert!(key.conperiod, "{table}: {key:?}");
    }
    // A temporal primary key's columns are NOT NULL; a UNIQUE one's aren't.
    let t = db.resolve_table(None, "t").unwrap();
    assert!(db.attributes_of(t.oid).iter().all(|a| a.attnotnull));
    let tu = db.resolve_table(None, "t_u").unwrap();
    assert!(db.attributes_of(tu.oid).iter().all(|a| !a.attnotnull));
    // Its index is a GiST exclusion one, still unique.
    let index = db
        .pg_index_values()
        .find(|i| i.indrelid == t.oid && i.indisprimary)
        .unwrap();
    assert!(index.indisunique && index.indisexclusion);
}

#[test]
fn without_overlaps_key_columns_are_checked() {
    for (stmt, msg) in [
        (
            "CREATE TABLE x (valid_at daterange, PRIMARY KEY (valid_at WITHOUT OVERLAPS));",
            "constraint using WITHOUT OVERLAPS needs at least two columns",
        ),
        (
            "CREATE TABLE x (id int4range, valid_at text, PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));",
            "column \"valid_at\" in WITHOUT OVERLAPS is not a range or multirange type",
        ),
        (
            "CREATE TABLE x (id int4range, PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));",
            "column \"valid_at\" named in key does not exist",
        ),
        (
            // Without btree_gist a scalar has no GiST operator class.
            "CREATE TABLE x (id int, valid_at daterange, PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));",
            "data type integer has no default operator class for access method \"gist\"",
        ),
        (
            "CREATE TABLE x (id int4range, valid_at daterange,
                PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)) PARTITION BY RANGE (valid_at);",
            "cannot match partition key to index on column \"valid_at\" using non-equal operator \"&&\"",
        ),
        (
            "CREATE TABLE x (a int, UNIQUE (a, a));",
            "column \"a\" appears twice in unique constraint",
        ),
    ] {
        assert_err("", stmt, msg);
    }
    let setup = "CREATE TABLE y (a int, b daterange);";
    for (stmt, msg) in [
        (
            "ALTER TABLE y ADD UNIQUE (b, a WITHOUT OVERLAPS);",
            "column \"a\" in WITHOUT OVERLAPS is not a range or multirange type",
        ),
        (
            "ALTER TABLE y ADD PRIMARY KEY (a, a);",
            "column \"a\" appears twice in primary key constraint",
        ),
        (
            "ALTER TABLE y ADD UNIQUE (nosuch);",
            "column \"nosuch\" named in key does not exist",
        ),
        (
            "ALTER TABLE y ADD PRIMARY KEY (nosuch);",
            "column \"nosuch\" of relation \"y\" does not exist",
        ),
    ] {
        assert_err(setup, stmt, msg);
    }
    build_db(&[(
        "0001.sql",
        "CREATE TABLE p (id int4range, valid_at daterange,
            PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)) PARTITION BY LIST (id);
         CREATE TABLE d (id int4range, valid_at daterange,
            PRIMARY KEY (id, valid_at WITHOUT OVERLAPS) DEFERRABLE);
         CREATE TABLE n (id int4range, valid_at daterange,
            UNIQUE NULLS NOT DISTINCT (id, valid_at WITHOUT OVERLAPS));",
    )]);
}

#[test]
fn period_foreign_keys_reference_temporal_keys() {
    let setup = format!(
        "{T} CREATE TABLE plain (id int4range, valid_at daterange, PRIMARY KEY (id, valid_at));"
    );
    let db = build_db(&[
        ("0001.sql", &setup),
        (
            "0002.sql",
            "CREATE TABLE ref (id int4range, valid_at daterange, parent_id int4range,
                FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES t (id, PERIOD valid_at));
             CREATE TABLE ref2 (id int4range, valid_at daterange, parent_id int4range,
                FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES t);
             CREATE TABLE ref12 (id int4range, valid_at daterange, parent_id int4range,
                FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES t (id, PERIOD valid_at) MATCH FULL);
             ALTER TABLE ref ADD CONSTRAINT rr FOREIGN KEY (parent_id, PERIOD valid_at)
                REFERENCES t (id, PERIOD valid_at) NOT VALID;",
        ),
    ]);
    let fks: Vec<_> = db
        .constraints_of_table("ref")
        .into_iter()
        .filter(|c| c.contype == ConType::ForeignKey)
        .collect();
    assert_eq!(fks.len(), 2);
    assert!(fks.iter().all(|c| c.conperiod));
    assert!(fks.iter().any(|c| c.conname == "rr" && !c.convalidated));

    let fk = |body: &str| {
        format!("CREATE TABLE r (id int4range, valid_at daterange, parent_id int4range, {body});")
    };
    for (body, msg) in [
        (
            "FOREIGN KEY (parent_id, valid_at) REFERENCES t (id, PERIOD valid_at)",
            "foreign key uses PERIOD on the referenced table but not the referencing table",
        ),
        (
            "FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES t (id, valid_at)",
            "foreign key uses PERIOD on the referencing table but not the referenced table",
        ),
        (
            "FOREIGN KEY (parent_id, valid_at) REFERENCES t (id, valid_at)",
            "foreign key must use PERIOD when referencing a primary key using WITHOUT OVERLAPS",
        ),
        (
            "FOREIGN KEY (parent_id, valid_at) REFERENCES t",
            "foreign key uses PERIOD on the referenced table but not the referencing table",
        ),
        (
            "FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES t (id, PERIOD valid_at) ON DELETE CASCADE",
            "unsupported ON DELETE action for foreign key constraint using PERIOD",
        ),
        (
            "FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES t (id, PERIOD valid_at) ON UPDATE RESTRICT",
            "unsupported ON UPDATE action for foreign key constraint using PERIOD",
        ),
        (
            "FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES plain (id, PERIOD valid_at)",
            "there is no unique constraint matching given keys for referenced table \"plain\"",
        ),
    ] {
        assert_err(&setup, &fk(body), msg);
    }
    for (valid_at, shown) in [
        ("int4range", "int4range"),
        ("datemultirange", "datemultirange"),
        ("text", "text"),
    ] {
        assert_err(
            &setup,
            &format!(
                "CREATE TABLE r (valid_at {valid_at}, parent_id int4range,
                    FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES t (id, PERIOD valid_at));"
            ),
            &format!(
                "foreign key constraint \"r_parent_id_valid_at_fkey\" cannot be implemented (Key \
                 columns \"valid_at\" of the referencing table and \"valid_at\" of the referenced \
                 table are of incompatible types: {shown} and daterange.)"
            ),
        );
    }
}

#[test]
fn on_conflict_does_not_infer_a_temporal_key() {
    // infer_arbiter_indexes: a WITHOUT OVERLAPS key is an exclusion
    // constraint.
    let db = build_db(&[("0001.sql", T)]);
    let row = "INSERT INTO t VALUES ('[1,2)', '[2020-01-01,2021-01-01)')";
    db.analyze(&format!("{row} ON CONFLICT DO NOTHING"))
        .unwrap();
    db.analyze(&format!("{row} ON CONFLICT ON CONSTRAINT t_pk DO NOTHING"))
        .unwrap();
    let err = db
        .analyze(&format!("{row} ON CONFLICT (id, valid_at) DO NOTHING"))
        .unwrap_err();
    assert!(
        err.to_string().starts_with(
            "there is no unique or exclusion constraint matching the ON CONFLICT specification"
        ),
        "{err}"
    );
    let err = db
        .analyze(&format!(
            "{row} ON CONFLICT ON CONSTRAINT t_pk DO UPDATE SET id = excluded.id"
        ))
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("ON CONFLICT DO UPDATE not supported with exclusion constraints"),
        "{err}"
    );
}
