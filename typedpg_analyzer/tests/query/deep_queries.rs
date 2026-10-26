//! Queries nested deep: as deep as PostgreSQL takes them (with its default
//! `max_stack_depth`, 2 MB), analyzed on a test thread's 2 MB of stack, in
//! time linear in their depth.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (x int NOT NULL);").unwrap();
    db
}

#[track_caller]
fn assert_int4(db: &PgCatalog, sql: &str) {
    let analyzed = db.analyze(sql).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(analyzed.columns[0].pg_type, int4());
}

#[test]
fn long_operator_chains() {
    // 47 terms already went past prost's decoding limit (100 levels).
    assert_int4(&setup(), &format!("SELECT 1{} AS v", " + 1".repeat(2000)));
}

#[test]
fn nested_subqueries() {
    // Each level was analyzed twice, so 2^depth times in all: 25 levels
    // took minutes.
    let n = 500;
    let sql = format!(
        "SELECT {}x{} AS v FROM t",
        "(SELECT ".repeat(n),
        ")".repeat(n)
    );
    assert_int4(&setup(), &sql);
}

#[test]
fn nested_calls() {
    // A one-argument call inferred its argument twice: 2^depth again.
    let n = 1000;
    assert_int4(
        &setup(),
        &format!("SELECT {}x{} AS v FROM t", "abs(".repeat(n), ")".repeat(n)),
    );
}

#[test]
fn long_set_operations() {
    let sql = format!(
        "SELECT x AS v FROM t{}",
        " UNION SELECT x FROM t".repeat(2000)
    );
    assert_int4(&setup(), &sql);
}

#[test]
fn too_deep_is_an_error() {
    // Past the 10,000 levels libpg_query takes (where PG has long run out
    // of stack): an error, not a crash.
    let err = setup()
        .analyze(&format!("SELECT 1{} AS v", " + 1".repeat(6000)))
        .unwrap_err();
    assert!(
        matches!(err, AnalyzeError::StatementTooComplex(_)),
        "{err:?}"
    );
    assert!(
        err.to_string().starts_with("stack depth limit exceeded"),
        "{err}"
    );
}
