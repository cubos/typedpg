//! `copy_in!` targets: `PgCatalog::analyze_copy_in`.

use crate::common::*;

fn db() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE SCHEMA \"My Schema\";
         CREATE TABLE \"My Schema\".t (
             id int GENERATED ALWAYS AS IDENTITY,
             name text NOT NULL,
             note text,
             g int GENERATED ALWAYS AS (id * 2) STORED
         );
         CREATE TABLE plain (a int, b text);
         CREATE VIEW v AS SELECT a FROM plain;
         CREATE VIEW vi AS SELECT a FROM plain;
         CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN INSERT INTO plain (a) VALUES (NEW.a); RETURN NEW; END $$;
         CREATE TRIGGER ti INSTEAD OF INSERT ON vi FOR EACH ROW EXECUTE FUNCTION trg();
         CREATE TABLE p (a int) PARTITION BY RANGE (a);",
    )
    .unwrap();
    db
}

#[test]
fn a_column_list_gives_the_columns_their_types_and_nullability() {
    let target = db()
        .analyze_copy_in("\"My Schema\".t (name, note, id)")
        .unwrap();
    assert_eq!(
        target.copy_sql,
        "COPY \"My Schema\".t (name, note, id) FROM STDIN (FORMAT binary)"
    );
    assert_eq!(
        target.describe_sql,
        "SELECT NULL::pg_catalog.text, NULL::pg_catalog.text, NULL::pg_catalog.int4"
    );
    let cols: Vec<_> = target
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    // An identity column takes COPY input; NOT NULL makes a value required.
    assert_eq!(cols, [("name", false), ("note", true), ("id", false)]);
}

#[test]
fn without_a_list_every_column_but_the_generated_ones() {
    let target = db().analyze_copy_in("\"My Schema\".t").unwrap();
    let names: Vec<_> = target.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "note"]);
}

#[test]
fn views_with_an_instead_of_insert_trigger_and_partitioned_tables_take_rows() {
    let db = db();
    db.analyze_copy_in("vi (a)").unwrap();
    db.analyze_copy_in("p (a)").unwrap();
}

#[test]
fn targets_are_checked_like_pg() {
    let db = db();
    for (target, message) in [
        (
            "\"My Schema\".t (name, g)",
            "column \"g\" is a generated column",
        ),
        ("v (a)", "cannot copy to view \"v\""),
        ("nosuch (a)", "relation \"nosuch\" does not exist"),
        (
            "plain (nosuch)",
            "column \"nosuch\" of relation \"plain\" does not exist",
        ),
        ("plain (a, a)", "column \"a\" specified more than once"),
        (
            "plain (a) FROM STDIN; DROP TABLE plain; --",
            "copy_in! target must be a table and an optional column list",
        ),
        (
            "plain (a) FROM STDIN WITH (FORMAT csv) --",
            "copy_in! target must be a table and an optional column list",
        ),
    ] {
        let err = db.analyze_copy_in(target).unwrap_err();
        assert!(
            err.to_string().starts_with(message),
            "{target}\n  got: {err}"
        );
    }
}
