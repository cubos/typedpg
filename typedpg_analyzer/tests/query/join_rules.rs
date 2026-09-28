//! JOIN USING / NATURAL merged columns and PG's join validation rules:
//! merged-column nullability, chained USING joins, `USING (…) AS alias`,
//! duplicate / ambiguous USING columns, LATERAL references under
//! RIGHT / FULL joins.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, b text, c varchar(10) NOT NULL);
         CREATE TABLE u (id int PRIMARY KEY, t_id int NOT NULL, x text NOT NULL, y int);
         CREATE TABLE nn (id int, q int NOT NULL);
         CREATE TABLE big (id bigint NOT NULL, z int);
         CREATE TABLE k (a int NOT NULL, b int NOT NULL, c text);",
    )
    .unwrap();
    db
}

// ── FULL JOIN merged-column nullability ──────────────────────────────────────

/// The merged column is `COALESCE(l.id, r.id)`: PG 18 returns a NULL `id`
/// for `nn`'s unmatched row with `id = NULL`, so it is only NOT NULL when
/// both sides are.
#[test]
fn full_join_using_merged_column_needs_both_sides_not_null() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM nn FULL JOIN u USING (id)")
        .unwrap();
    assert_cols(&s, vec![cn("id", int4())]);
    let s = db
        .analyze("SELECT id FROM u FULL JOIN nn USING (id)")
        .unwrap();
    assert_cols(&s, vec![cn("id", int4())]);
    let s = db.analyze("SELECT id FROM nn NATURAL FULL JOIN u").unwrap();
    assert_cols(&s, vec![cn("id", int4())]);
    let s = db
        .analyze("SELECT * FROM nn FULL JOIN u USING (id)")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("id", int4()),
            cn("q", int4()),
            cn("t_id", int4()),
            cn("x", text()),
            cn("y", int4()),
        ],
    );
    // Both sides NOT NULL → NOT NULL.
    let s = db
        .analyze("SELECT id FROM t FULL JOIN u USING (id)")
        .unwrap();
    assert_cols(&s, vec![c("id", int4())]);
}

/// A merged column inherits the outer-join nullability its constituent
/// already had from an earlier join.
#[test]
fn merged_column_keeps_inner_outer_join_nullability() {
    let db = setup();
    // `t.a` is NOT NULL in its table but NULL for `nn` rows without a `t`
    // match; the LEFT JOIN's merged `a` is the left value.
    let s = db
        .analyze("SELECT a FROM nn LEFT JOIN t ON true LEFT JOIN k USING (a)")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4())]);
    // An INNER join's strict `=` discards NULLs, so a NOT NULL side suffices.
    let s = db.analyze("SELECT id FROM nn JOIN u USING (id)").unwrap();
    assert_cols(&s, vec![c("id", int4())]);
}

// ── Chained USING joins ──────────────────────────────────────────────────────

#[test]
fn chained_full_join_using() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM nn FULL JOIN u USING (id) FULL JOIN t USING (id)")
        .unwrap();
    assert_cols(&s, vec![cn("id", int4())]);
    // PG 18: the outer merge resolves int and bigint to bigint.
    let s = db
        .analyze("SELECT * FROM (t JOIN u USING (id)) JOIN big USING (id)")
        .unwrap();
    assert_eq!(s.columns[0].name, "id");
    assert_eq!(s.columns[0].pg_type, int8());
    assert_eq!(s.columns.len(), 8);
}

/// A correlated sublink sees the merged column, not two ambiguous
/// constituents.
#[test]
fn sublink_sees_merged_using_column() {
    let db = setup();
    let s = db
        .analyze("SELECT (SELECT id) AS v FROM t JOIN u USING (id)")
        .unwrap();
    assert_cols(&s, vec![cn("v", int4())]);
}

// ── USING (…) AS alias (PG 14) ───────────────────────────────────────────────

#[test]
fn join_using_alias_exposes_the_merged_columns() {
    let db = setup();
    let s = db
        .analyze("SELECT j.id FROM t FULL JOIN u USING (id) AS j")
        .unwrap();
    assert_cols(&s, vec![c("id", int4())]);
    let s = db
        .analyze("SELECT j.* FROM t JOIN u USING (id) AS j")
        .unwrap();
    assert_cols(&s, vec![c("id", int4())]);
    let s = db
        .analyze("SELECT j.id FROM (t JOIN u USING (id) AS j) JOIN big USING (id)")
        .unwrap();
    assert_cols(&s, vec![c("id", int4())]);
    let err = db
        .analyze("SELECT j.a FROM t JOIN u USING (id) AS j")
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::UndefinedColumn(_)), "{err:?}");
    assert!(
        err.to_string().starts_with("column j.a does not exist"),
        "{err}"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM t JOIN u USING (id) AS t"),
        AnalyzeError::DuplicateAlias(_),
        "table name \"t\" specified more than once"
    );
}

// ── Validation ───────────────────────────────────────────────────────────────

#[test]
fn duplicate_using_column_is_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT * FROM t JOIN big USING (id, id)"),
        AnalyzeError::DuplicateColumn(_),
        "column name \"id\" appears more than once in USING clause"
    );
}

#[test]
fn ambiguous_common_column_is_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT * FROM t JOIN u ON true JOIN big USING (id)"),
        AnalyzeError::AmbiguousColumn(_),
        "common column name \"id\" appears more than once in left table"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM big JOIN (t JOIN u ON true) USING (id)"),
        AnalyzeError::AmbiguousColumn(_),
        "common column name \"id\" appears more than once in right table"
    );
}

/// PG 18 (42P10): `The combining JOIN type must be INNER or LEFT for a
/// LATERAL reference.`
#[test]
fn lateral_reference_under_right_or_full_join_is_rejected() {
    let db = setup();
    for sql in [
        "SELECT * FROM t RIGHT JOIN LATERAL (SELECT t.a AS z) s ON true",
        "SELECT * FROM t FULL JOIN LATERAL (SELECT t.a AS z) s ON true",
        "SELECT * FROM t RIGHT JOIN LATERAL (SELECT a AS z) s ON true",
        "SELECT * FROM t RIGHT JOIN generate_series(1, t.a) g ON true",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::InvalidColumnReference(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("invalid reference to FROM-clause entry for table \"t\""),
            "{sql}: {err}"
        );
    }
    // INNER / LEFT are fine.
    db.analyze("SELECT * FROM t LEFT JOIN LATERAL (SELECT t.a AS z) s ON true")
        .unwrap();
}

#[test]
fn join_error_wordings_match_pg() {
    let db = setup();
    let err = db
        .analyze("SELECT * FROM t JOIN u ON sum(t.a) > 0")
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::GroupingError(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("aggregate functions are not allowed in JOIN conditions"),
        "{err}"
    );
    let err = db.analyze("SELECT * FROM t NATURAL JOIN k").unwrap_err();
    assert!(matches!(err, AnalyzeError::UndefinedOperator(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("operator does not exist: text = integer"),
        "{err}"
    );
}

// ── Unaliased FROM subqueries (PG 16) ────────────────────────────────────────

/// Several unaliased subqueries get no referencable name, so they don't
/// clash with each other.
#[test]
fn several_unaliased_subqueries() {
    let db = setup();
    let s = db.analyze("SELECT * FROM (SELECT 1), (SELECT 2)").unwrap();
    assert_cols(&s, vec![c("?column?", int4()), c("?column?", int4())]);
    let s = db
        .analyze("SELECT a, x FROM (SELECT a FROM t) JOIN (SELECT x FROM u) ON true")
        .unwrap();
    assert_cols(&s, vec![c("a", int4()), c("x", text())]);
}
