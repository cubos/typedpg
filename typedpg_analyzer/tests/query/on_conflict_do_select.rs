//! PostgreSQL 19's `INSERT ... ON CONFLICT ... DO SELECT [FOR UPDATE /
//! SHARE] [WHERE ...] RETURNING ...`: the conflicting row is returned
//! instead of being updated (or skipped).
//!
//! A returned row is an inserted one (no OLD) or an existing one (OLD and
//! NEW are both that row), so RETURNING's nullability is what holds of
//! both.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL, w text);
         CREATE TABLE ex (r int4range, EXCLUDE USING gist (r WITH &&));",
    )
    .unwrap();
    db
}

#[test]
fn returning_holds_for_inserted_and_existing_rows() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO t VALUES (1, 9, 'z') ON CONFLICT (id) DO SELECT \
             RETURNING id, v, w, old.v AS ov, new.v AS nv",
        )
        .unwrap();
    // `w` is 'z' in an inserted row, but may be NULL in an existing one.
    assert_cols(
        &s,
        vec![
            c("id", int4()),
            c("v", int4()),
            cn("w", text()),
            cn("ov", int4()),
            c("nv", int4()),
        ],
    );
    // The DO SELECT WHERE filters the existing rows it returns.
    let s = db
        .analyze(
            "INSERT INTO t VALUES (1, 9, 'z') ON CONFLICT (id) \
             DO SELECT WHERE t.w IS NOT NULL RETURNING w",
        )
        .unwrap();
    assert_cols(&s, vec![c("w", text())]);
    // ... which an inserted row doesn't go through.
    let s = db
        .analyze(
            "INSERT INTO t VALUES (1, 9, NULL) ON CONFLICT (id) \
             DO SELECT WHERE t.w IS NOT NULL RETURNING w",
        )
        .unwrap();
    assert_cols(&s, vec![cn("w", text())]);
}

#[test]
fn locking_and_excluded_in_the_where_clause() {
    let db = setup();
    for sql in [
        "INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO SELECT FOR UPDATE RETURNING id",
        "INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO SELECT FOR NO KEY UPDATE RETURNING id",
        "INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO SELECT FOR SHARE RETURNING id",
        "INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO SELECT FOR KEY SHARE RETURNING id",
        "INSERT INTO t VALUES (1, 9) ON CONFLICT ON CONSTRAINT t_pkey DO SELECT RETURNING *",
        "INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO SELECT \
         WHERE t.v < excluded.v RETURNING id",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn do_select_requires_returning_and_an_arbiter() {
    let db = setup();
    assert_err_prefix!(
        db.analyze("INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO SELECT"),
        AnalyzeError::SyntaxError(_),
        "ON CONFLICT DO SELECT requires a RETURNING clause"
    );
    assert_err_prefix!(
        db.analyze("INSERT INTO t VALUES (1, 9) ON CONFLICT DO SELECT RETURNING id"),
        AnalyzeError::SyntaxError(_),
        "ON CONFLICT DO SELECT requires inference specification or constraint name"
    );
}

#[test]
fn excluded_is_not_visible_to_returning() {
    let db = setup();
    assert_err_prefix!(
        db.analyze("INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO SELECT RETURNING excluded.v",),
        AnalyzeError::UndefinedTable(_),
        "invalid reference to FROM-clause entry for table \"excluded\""
    );
}

#[test]
fn where_clause_rejects_aggregates() {
    let db = setup();
    assert_err_prefix!(
        db.analyze(
            "INSERT INTO t VALUES (1, 9) ON CONFLICT (id) \
             DO SELECT WHERE count(*) > 0 RETURNING id",
        ),
        AnalyzeError::GroupingError(_),
        "aggregate functions are not allowed in WHERE"
    );
}

#[test]
fn exclusion_constraints_cannot_arbitrate() {
    let db = setup();
    assert_err_prefix!(
        db.analyze(
            "INSERT INTO ex VALUES ('[1,2)') ON CONFLICT ON CONSTRAINT ex_r_excl \
             DO SELECT RETURNING r",
        ),
        AnalyzeError::WrongObjectType(_),
        "ON CONFLICT DO SELECT not supported with exclusion constraints"
    );
}

#[test]
fn where_clause_names_its_clause_when_not_boolean() {
    // transformOnConflictClause transforms the DO SELECT / DO UPDATE WHERE
    // with transformWhereClause, which coerces it as WHERE.
    let db = setup();
    assert_err_prefix!(
        db.analyze("INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO SELECT WHERE 1 RETURNING id"),
        AnalyzeError::DatatypeMismatch(_),
        "argument of WHERE must be type boolean, not type integer"
    );
    assert_err_prefix!(
        db.analyze("INSERT INTO t VALUES (1, 9) ON CONFLICT (id) DO UPDATE SET v = 2 WHERE 1"),
        AnalyzeError::DatatypeMismatch(_),
        "argument of WHERE must be type boolean, not type integer"
    );
}
