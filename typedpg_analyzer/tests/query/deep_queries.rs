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
fn nested_subqueries_typing_a_parameter() {
    // Each level's select list is replayed before its WHERE (PG's
    // transform order), and the final pass does not replay a subquery the
    // replay analyzed: quadratic in the depth, where replaying at every
    // level would be 2^depth.
    let n = 50;
    let sql = format!(
        "SELECT {}$1::int + x{} AS v FROM t",
        "(SELECT ".repeat(n),
        " FROM t WHERE x = $1)".repeat(n)
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

#[test]
fn a_self_recursive_view_is_an_error_not_a_crash() {
    // PG accepts the CREATE OR REPLACE (regress/sql/lock.sql) and fails
    // the query. The analyzer recursed until the process aborted — inside
    // `sql!`, rustc itself.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (a int);
         CREATE VIEW v2 AS SELECT a FROM t;
         CREATE VIEW v3 AS SELECT a FROM v2;
         CREATE OR REPLACE VIEW v2 AS SELECT a FROM v3;
         -- A view definition isn't expanded: storing one over the loop is fine.
         CREATE VIEW v4 AS SELECT * FROM v2;",
    )
    .unwrap();
    // fireRIRrules names the view it meets a second time.
    for (sql, relation) in [
        ("SELECT * FROM v2", "v2"),
        ("SELECT a FROM v3", "v3"),
        ("SELECT * FROM v4", "v2"),
        ("SELECT (SELECT a FROM v2)", "v2"),
        ("SELECT * FROM t WHERE a IN (SELECT a FROM v3)", "v3"),
        ("SELECT * FROM v2 FOR UPDATE", "v2"),
        ("UPDATE v2 SET a = 1", "v2"),
        ("INSERT INTO v2 VALUES (1)", "v2"),
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(
            matches!(err, AnalyzeError::InvalidObjectDefinition(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string().starts_with(&format!(
                "infinite recursion detected in rules for relation \"{relation}\""
            )),
            "{sql}: {err}"
        );
    }
    // A CTE of the same name is no view.
    db.analyze("WITH v2 AS (SELECT 1 AS a) SELECT * FROM v2")
        .unwrap();
    // The rules a statement fires are rewritten too.
    db.apply_sql(
        "CREATE TABLE r (a int);
         CREATE RULE rr AS ON INSERT TO r DO ALSO SELECT * FROM v3;",
    )
    .unwrap();
    let err = db.analyze("INSERT INTO r VALUES (1)").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("infinite recursion detected in rules for relation \"v3\""),
        "{err}"
    );
    db.analyze("DELETE FROM r").unwrap();
}
