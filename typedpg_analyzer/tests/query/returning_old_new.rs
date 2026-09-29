//! PostgreSQL 18's `RETURNING old.* / new.*` and `RETURNING WITH (OLD AS o,
//! NEW AS n)` in INSERT (incl. ON CONFLICT), UPDATE, DELETE and MERGE.
//!
//! Expectations were observed on a live PostgreSQL 18.
//! `transformReturningClause` (parser/analyze.c) adds the OLD / NEW rows as
//! table-only namespace items over the target relation, unless a relation
//! already visible at this level has that name. `old` is NULL where no old
//! row exists (INSERT, the insert arm of ON CONFLICT / MERGE) and `new` where
//! no new row exists (DELETE, MERGE's DELETE arm) — even for NOT NULL columns.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL, w text);
         CREATE TABLE u (id int, x text);
         CREATE TABLE old (id int, z int);
         CREATE TABLE z ();",
    )
    .unwrap();
    db
}

fn t_row() -> Type {
    composite(
        "public",
        "t",
        vec![rf("id", int4()), rf("v", int4()), rfn("w", text())],
    )
}

// ── Columns and stars ────────────────────────────────────────────────────────

#[test]
fn update_returns_old_and_new_columns() {
    let db = setup();
    let s = db
        .analyze("UPDATE t SET v = v + 1 RETURNING old.v AS ov, new.v AS nv, old.*, new.w")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("ov", int4()),
            c("nv", int4()),
            c("id", int4()),
            c("v", int4()),
            cn("w", text()),
            cn("w", text()),
        ],
    );
}

#[test]
fn insert_old_row_is_null() {
    let db = setup();
    let s = db
        .analyze("INSERT INTO t VALUES (1, 2, 'b') RETURNING old.*, new.*")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("id", int4()),
            cn("v", int4()),
            cn("w", text()),
            c("id", int4()),
            c("v", int4()),
            cn("w", text()),
        ],
    );
}

#[test]
fn delete_new_row_is_null() {
    let db = setup();
    let s = db
        .analyze("DELETE FROM t RETURNING old.v AS ov, new.v AS nv, new.*")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("ov", int4()),
            cn("nv", int4()),
            cn("id", int4()),
            cn("v", int4()),
            cn("w", text()),
        ],
    );
}

#[test]
fn whole_row_old_and_new() {
    let db = setup();
    let s = db
        .analyze("UPDATE t SET v = 1 RETURNING old, new, (old).v, (new).w")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("old", t_row()),
            c("new", t_row()),
            c("v", int4()),
            cn("w", text()),
        ],
    );
    let s = db
        .analyze("INSERT INTO t VALUES (3, 3, 'b') RETURNING old, new, (old).v, old IS NULL")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("old", t_row()),
            c("new", t_row()),
            cn("v", int4()),
            c("?column?", bool_ty()),
        ],
    );
    let s = db.analyze("DELETE FROM t RETURNING new").unwrap();
    assert_cols(&s, vec![cn("new", t_row())]);
}

#[test]
fn system_columns_of_old_and_new() {
    let db = setup();
    let s = db
        .analyze("UPDATE t SET v = v RETURNING old.xmin, old.ctid, new.tableoid")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("xmin", basic("pg_catalog", "xid")),
            c("ctid", basic("pg_catalog", "tid")),
            c("tableoid", oid_ty()),
        ],
    );
    // No old row for an INSERT: its system columns are NULL too.
    let s = db
        .analyze("INSERT INTO t VALUES (90, 9, 'x') RETURNING old.ctid, new.ctid")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("ctid", basic("pg_catalog", "tid")),
            c("ctid", basic("pg_catalog", "tid")),
        ],
    );
}

#[test]
fn bare_star_and_unqualified_names_ignore_old_and_new() {
    // OLD / NEW are table-only namespace items: `*` and bare column names
    // never reach them, so `v` is not ambiguous.
    let db = setup();
    let s = db
        .analyze("UPDATE t SET v = 5 RETURNING *, v, old.v, new.v, t.v")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int4()),
            c("v", int4()),
            cn("w", text()),
            c("v", int4()),
            c("v", int4()),
            c("v", int4()),
            c("v", int4()),
        ],
    );
}

#[test]
fn old_in_returning_subquery_and_params() {
    let db = setup();
    let s = db
        .analyze(
            "UPDATE t SET v = $p1 RETURNING old.v + $p2 AS d, \
             (SELECT count(*) FROM u WHERE u.id = old.id) AS n",
        )
        .unwrap();
    assert_cols(&s, vec![c("d", int4()), c("n", int8())]);
    assert_params(&s, vec![p(int4()), p(int4())]);
}

#[test]
fn old_and_new_in_data_modifying_cte() {
    let db = setup();
    let s = db
        .analyze(
            "WITH x AS (UPDATE t SET v = v + 1 RETURNING old.v AS a, new.v AS b) \
             SELECT * FROM x",
        )
        .unwrap();
    assert_cols(&s, vec![c("a", int4()), c("b", int4())]);
    // A CTE named `old` is not a namespace item, so it does not mask OLD.
    let s = db
        .analyze("WITH old AS (SELECT 1 AS q) UPDATE t SET v = v + 1 RETURNING old.v")
        .unwrap();
    assert_cols(&s, vec![c("v", int4())]);
}

#[test]
fn unknown_old_column() {
    let db = setup();
    assert_err_prefix!(
        db.analyze("UPDATE t SET v = v RETURNING old.nosuch"),
        AnalyzeError::UndefinedColumn(_),
        "column old.nosuch does not exist"
    );
}

// ── ON CONFLICT ──────────────────────────────────────────────────────────────

#[test]
fn on_conflict_do_update_old_row_is_nullable() {
    let db = setup();
    let s = db
        .analyze(
            "INSERT INTO t VALUES (1, 9, 'z') ON CONFLICT (id) \
             DO UPDATE SET v = excluded.v RETURNING old.v AS ov, new.v AS nv",
        )
        .unwrap();
    assert_cols(&s, vec![cn("ov", int4()), c("nv", int4())]);
    let s = db
        .analyze(
            "INSERT INTO t VALUES (1, 9, 'z') ON CONFLICT (id) DO NOTHING \
             RETURNING old.v AS ov, new.v AS nv",
        )
        .unwrap();
    assert_cols(&s, vec![cn("ov", int4()), c("nv", int4())]);
}

#[test]
fn on_conflict_returning_excluded_is_invalid_reference() {
    let db = setup();
    assert_err_prefix!(
        db.analyze(
            "INSERT INTO t VALUES (1, 9, 'z') ON CONFLICT (id) \
             DO UPDATE SET v = excluded.v RETURNING excluded.v",
        ),
        AnalyzeError::UndefinedTable(_),
        "invalid reference to FROM-clause entry for table \"excluded\""
    );
    // … but EXCLUDED isn't a namespace item there, so OLD may take its name.
    let s = db
        .analyze(
            "INSERT INTO t VALUES (1, 9, 'z') ON CONFLICT (id) \
             DO UPDATE SET v = 1 RETURNING WITH (OLD AS excluded) excluded.v",
        )
        .unwrap();
    assert_cols(&s, vec![cn("v", int4())]);
}

#[test]
fn on_conflict_target_alias_masks_old_or_new() {
    let db = setup();
    // `t AS old`: `old` is the target (the new values); NEW keeps its name.
    let s = db
        .analyze(
            "INSERT INTO t AS old VALUES (1, 9, 'z') ON CONFLICT (id) \
             DO UPDATE SET v = old.v + 1 RETURNING old.v AS a, new.v AS b",
        )
        .unwrap();
    assert_cols(&s, vec![c("a", int4()), c("b", int4())]);
    let s = db
        .analyze(
            "INSERT INTO t AS new VALUES (1, 9, 'z') ON CONFLICT (id) \
             DO UPDATE SET v = new.v + 1 RETURNING old.v AS a, new.v AS b",
        )
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), c("b", int4())]);
}

// ── MERGE ────────────────────────────────────────────────────────────────────

#[test]
fn merge_old_null_for_insert_arm() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO t USING (VALUES (1, 7)) s(id, v) ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v, 'm') \
             RETURNING merge_action(), old.v AS ov, new.v AS nv, old, new",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("merge_action", text()),
            cn("ov", int4()),
            c("nv", int4()),
            cn("old", t_row()),
            c("new", t_row()),
        ],
    );
}

#[test]
fn merge_new_null_for_delete_arm() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO t USING (VALUES (1, 7)) s(id, v) ON t.id = s.id \
             WHEN MATCHED THEN DELETE RETURNING old.v AS ov, new.v AS nv",
        )
        .unwrap();
    assert_cols(&s, vec![c("ov", int4()), cn("nv", int4())]);
    let s = db
        .analyze(
            "MERGE INTO t USING (VALUES (1, 7)) s(id, v) ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = 3 RETURNING old.v AS ov, new.v AS nv",
        )
        .unwrap();
    assert_cols(&s, vec![c("ov", int4()), c("nv", int4())]);
}

#[test]
fn merge_source_alias_masks_old() {
    let db = setup();
    let s = db
        .analyze(
            "MERGE INTO t USING (VALUES (1, 7)) old(id, q) ON t.id = old.id \
             WHEN MATCHED THEN UPDATE SET v = 3 RETURNING old.q, new.v",
        )
        .unwrap();
    assert_cols(&s, vec![c("q", int4()), c("v", int4())]);
    assert_err_prefix!(
        db.analyze(
            "MERGE INTO t USING (VALUES (1, 7)) old(id, q) ON t.id = old.id \
             WHEN MATCHED THEN UPDATE SET v = 3 RETURNING old.v",
        ),
        AnalyzeError::UndefinedColumn(_),
        "column old.v does not exist"
    );
    assert_err_prefix!(
        db.analyze(
            "MERGE INTO t USING (VALUES (1, 7)) s(id, v) ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = 3 RETURNING WITH (OLD AS s) 1",
        ),
        AnalyzeError::DuplicateAlias(_),
        "table name \"s\" specified more than once"
    );
}

// ── RETURNING WITH (OLD AS …, NEW AS …) ──────────────────────────────────────

#[test]
fn returning_with_renames_old_and_new() {
    let db = setup();
    let s = db
        .analyze("UPDATE t SET v = 5 RETURNING WITH (OLD AS o, NEW AS n) o.v AS a, n.v AS b, o.*")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", int4()),
            c("b", int4()),
            c("id", int4()),
            c("v", int4()),
            cn("w", text()),
        ],
    );
    let s = db
        .analyze("DELETE FROM t RETURNING WITH (NEW AS n) n.v, new.v")
        .unwrap_err();
    assert!(
        s.to_string()
            .starts_with("missing FROM-clause entry for table \"new\""),
        "{s}"
    );
    let s = db
        .analyze("DELETE FROM t RETURNING WITH (NEW AS n) n.v AS a, old.v AS b")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), c("b", int4())]);
}

#[test]
fn returning_with_errors() {
    let db = setup();
    assert_err_prefix!(
        db.analyze("UPDATE t SET v = 5 RETURNING WITH (OLD AS o) old.v"),
        AnalyzeError::UndefinedTable(_),
        "missing FROM-clause entry for table \"old\""
    );
    assert_err_prefix!(
        db.analyze("UPDATE t SET v = 5 RETURNING WITH (OLD AS o, OLD AS p) o.v"),
        AnalyzeError::SyntaxError(_),
        "OLD cannot be specified multiple times"
    );
    assert_err_prefix!(
        db.analyze("UPDATE t SET v = 5 RETURNING WITH (NEW AS o, NEW AS p) o.v"),
        AnalyzeError::SyntaxError(_),
        "NEW cannot be specified multiple times"
    );
    assert_err_prefix!(
        db.analyze("UPDATE t SET v = 5 RETURNING WITH (OLD AS o, NEW AS o) o.v"),
        AnalyzeError::DuplicateAlias(_),
        "table name \"o\" specified more than once"
    );
    assert_err_prefix!(
        db.analyze("UPDATE t SET v = 5 RETURNING WITH (OLD AS t) t.v"),
        AnalyzeError::DuplicateAlias(_),
        "table name \"t\" specified more than once"
    );
    assert_err_prefix!(
        db.analyze("UPDATE t SET v = 5 FROM u WHERE t.id = u.id RETURNING WITH (OLD AS u) u.v"),
        AnalyzeError::DuplicateAlias(_),
        "table name \"u\" specified more than once"
    );
    assert_err_prefix!(
        db.analyze(
            "UPDATE t SET v = 5 FROM old WHERE t.id = old.id \
             RETURNING WITH (OLD AS old) old.v"
        ),
        AnalyzeError::DuplicateAlias(_),
        "table name \"old\" specified more than once"
    );
}

// ── Masking by relations named old / new ─────────────────────────────────────

#[test]
fn from_item_named_old_masks_old() {
    let db = setup();
    let s = db
        .analyze("UPDATE t SET v = 5 FROM old WHERE t.id = old.id RETURNING old.z")
        .unwrap();
    assert_cols(&s, vec![cn("z", int4())]);
    assert_err_prefix!(
        db.analyze("UPDATE t SET v = 5 FROM old WHERE t.id = old.id RETURNING old.v"),
        AnalyzeError::UndefinedColumn(_),
        "column old.v does not exist"
    );
    // A subquery's own `old` shadows the outer OLD row.
    assert_err_prefix!(
        db.analyze("UPDATE t SET v = 5 RETURNING (SELECT old.v FROM u AS old LIMIT 1)"),
        AnalyzeError::UndefinedColumn(_),
        "column old.v does not exist"
    );
}

#[test]
fn target_alias_masks_old_or_new() {
    let db = setup();
    let s = db
        .analyze("UPDATE t AS old SET v = 5 RETURNING old.v AS a, new.v AS b")
        .unwrap();
    assert_cols(&s, vec![c("a", int4()), c("b", int4())]);
    let s = db
        .analyze("DELETE FROM t AS new RETURNING old.v AS a, new.v AS b")
        .unwrap();
    assert_cols(&s, vec![c("a", int4()), c("b", int4())]);
}

// ── OLD / NEW exist only in RETURNING ────────────────────────────────────────

#[test]
fn old_and_new_are_not_visible_outside_returning() {
    let db = setup();
    for sql in [
        "UPDATE t SET v = old.v RETURNING 1",
        "UPDATE t SET v = 1 WHERE old.v = 1 RETURNING 1",
        "DELETE FROM t WHERE old.v = 1",
        "INSERT INTO t VALUES (5, old.v, 'q') RETURNING 1",
        "INSERT INTO t VALUES (1, 9, 'z') ON CONFLICT (id) DO UPDATE SET v = old.v RETURNING 1",
        "INSERT INTO t VALUES (1, 9, 'z') ON CONFLICT (id) DO UPDATE SET v = 1 \
         WHERE old.v = 1 RETURNING 1",
        "MERGE INTO t USING (VALUES (1, 7)) s(id, v) ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET v = old.v RETURNING 1",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::UndefinedTable(_),
            "missing FROM-clause entry for table \"old\""
        );
    }
}

#[test]
fn returning_needs_at_least_one_column() {
    let db = setup();
    for sql in [
        "INSERT INTO z DEFAULT VALUES RETURNING old.*",
        "INSERT INTO z DEFAULT VALUES RETURNING *",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::SyntaxError(_),
            "RETURNING must have at least one column"
        );
    }
}

// ── Rule actions: the rule's OLD / NEW mask RETURNING's ──────────────────────

#[test]
fn rule_action_returning_old_is_the_rules_old() {
    let mut db = setup();
    db.apply_sql(
        "CREATE VIEW tv AS SELECT * FROM t;
         CREATE RULE r AS ON UPDATE TO tv DO INSTEAD
           UPDATE t SET v = new.v WHERE id = old.id RETURNING old.*;",
    )
    .unwrap();
    db.apply_sql(
        "DROP RULE r ON tv;
         CREATE RULE r AS ON UPDATE TO tv DO INSTEAD
           UPDATE t SET v = new.v WHERE id = old.id RETURNING WITH (OLD AS o) o.*;",
    )
    .unwrap();
    let err = db
        .apply_sql(
            "DROP RULE r ON tv;
             CREATE RULE r AS ON UPDATE TO tv DO INSTEAD
               UPDATE t SET v = new.v WHERE id = old.id RETURNING WITH (OLD AS old) old.*;",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("table name \"old\" specified more than once"),
        "{err}"
    );
}
