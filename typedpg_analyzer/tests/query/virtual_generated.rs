//! PostgreSQL 18's virtual generated columns (`GENERATED ALWAYS AS (expr)
//! [VIRTUAL]`, the default without STORED) on the query side. Expectations
//! were observed on a live PostgreSQL 18: a virtual column reads like any
//! column — its declared type, typmod and NOT NULL — and, like a stored
//! one, can only be written as DEFAULT.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE g (
            id int PRIMARY KEY,
            a numeric NOT NULL,
            b numeric(6,2) GENERATED ALWAYS AS (a),
            e varchar(3) GENERATED ALWAYS AS ('ab') VIRTUAL,
            n int NOT NULL GENERATED ALWAYS AS (1),
            d int GENERATED ALWAYS AS (id + 1) STORED
         );",
    )
    .unwrap();
    db
}

#[test]
fn reading_virtual_columns() {
    let db = setup();
    let s = db.analyze("SELECT b, e, n FROM g").unwrap();
    assert_cols(
        &s,
        vec![
            cn("b", numeric_ps(6, 2)),
            cn("e", varchar_n(3)),
            c("n", int4()),
        ],
    );
}

#[test]
fn returning_virtual_columns() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO g (id, a) VALUES (1, 1.234) RETURNING b, e, old.b AS ob, new.n AS nn")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("b", numeric_ps(6, 2)),
            cn("e", varchar_n(3)),
            cn("ob", numeric_ps(6, 2)),
            c("nn", int4()),
        ],
    );
    let s = db
        .analyze("UPDATE g SET a = 5, e = DEFAULT RETURNING old.b AS ob, new.b AS nb")
        .unwrap();
    assert_cols(
        &s,
        vec![cn("ob", numeric_ps(6, 2)), cn("nb", numeric_ps(6, 2))],
    );
}

#[test]
fn writing_virtual_columns_is_rejected() {
    let db = setup();
    for (sql, msg) in [
        (
            "INSERT INTO g VALUES (1, 1, 2)",
            "cannot insert a non-DEFAULT value into column \"b\"",
        ),
        (
            "INSERT INTO g (id, a, e) VALUES (1, 1, 'x')",
            "cannot insert a non-DEFAULT value into column \"e\"",
        ),
        (
            "INSERT INTO g (id, a, b) VALUES (1, 1, DEFAULT), (2, 2, 5)",
            "cannot insert a non-DEFAULT value into column \"b\"",
        ),
        (
            "INSERT INTO g OVERRIDING SYSTEM VALUE VALUES (1, 1, 2)",
            "cannot insert a non-DEFAULT value into column \"b\"",
        ),
        (
            "INSERT INTO g (id, a, b) SELECT 1, 1, 2",
            "cannot insert a non-DEFAULT value into column \"b\"",
        ),
        (
            "INSERT INTO g (id, a, d) SELECT 1, 1, 2",
            "cannot insert a non-DEFAULT value into column \"d\"",
        ),
        (
            "UPDATE g SET b = 3",
            "column \"b\" can only be updated to DEFAULT",
        ),
        (
            "UPDATE g SET (a, b) = (1, 2)",
            "column \"b\" can only be updated to DEFAULT",
        ),
        (
            "UPDATE g SET (a, b) = (SELECT 1, 2)",
            "column \"b\" can only be updated to DEFAULT",
        ),
        (
            "MERGE INTO g USING (VALUES (1)) s(x) ON g.id = s.x \
             WHEN MATCHED THEN UPDATE SET b = 1",
            "column \"b\" can only be updated to DEFAULT",
        ),
        (
            "MERGE INTO g USING (VALUES (9)) s(x) ON g.id = s.x \
             WHEN NOT MATCHED THEN INSERT (id, a, b) VALUES (9, 1, 1)",
            "cannot insert a non-DEFAULT value into column \"b\"",
        ),
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
}

#[test]
fn writing_default_to_virtual_columns() {
    let db = setup();
    for sql in [
        "INSERT INTO g (id, a, b, e) VALUES (1, 1, DEFAULT, DEFAULT)",
        "INSERT INTO g VALUES (1, 1)",
        "UPDATE g SET (a, b) = (1, DEFAULT)",
        "UPDATE g SET b = DEFAULT",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
