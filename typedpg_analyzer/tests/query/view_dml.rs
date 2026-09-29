//! INSERT / UPDATE / DELETE / MERGE on views and on relations with rules —
//! the rewriter's checks (PG's RewriteQuery / rewriteTargetView /
//! rewriteTargetListIU): automatic updatability, non-updatable view
//! columns, generated and identity columns of the base relation reached
//! through a view, INSTEAD OF triggers, and rules.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE b (
             id int,
             a int,
             g int GENERATED ALWAYS AS (a * 2) STORED,
             v int GENERATED ALWAYS AS (a * 3) VIRTUAL,
             i int GENERATED ALWAYS AS IDENTITY
         );
         CREATE VIEW vb AS SELECT id, a, g, v, i FROM b;
         CREATE VIEW vr AS SELECT id, g AS gg, i AS ii, a + 1 AS a1 FROM b;
         CREATE VIEW vv AS SELECT * FROM vr;
         CREATE VIEW vg AS SELECT a, count(*) FROM b GROUP BY a;
         CREATE VIEW vs AS SELECT ctid AS c, b AS w, id FROM b;
         CREATE VIEW vt AS SELECT * FROM b TABLESAMPLE system (1);
         CREATE VIEW vx AS SELECT a + 1 AS x FROM b;
         CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;",
    )
    .unwrap();
    db
}

#[track_caller]
fn assert_prefix(db: &PgCatalog, sql: &str, msg: &str) -> AnalyzeError {
    let err = db
        .analyze(sql)
        .expect_err(&format!("expected an error for: {sql}"));
    assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    err
}

#[track_caller]
fn assert_ok(db: &PgCatalog, sql: &str) {
    db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// rewriteTargetListIU runs again on the base relation: its generated and
/// GENERATED ALWAYS identity columns only take DEFAULT through a view.
#[test]
fn generated_base_columns_through_views() {
    let db = setup();
    for (sql, msg) in [
        (
            "INSERT INTO vb (id, a, g) VALUES (1, 1, 1)",
            "cannot insert a non-DEFAULT value into column \"g\"",
        ),
        (
            "INSERT INTO vb (id, a, v) VALUES (1, 1, 1)",
            "cannot insert a non-DEFAULT value into column \"v\"",
        ),
        (
            "INSERT INTO vb (id, a, i) VALUES (1, 1, 1)",
            "cannot insert a non-DEFAULT value into column \"i\"",
        ),
        (
            "INSERT INTO vr (id, gg) VALUES (1, 1)",
            "cannot insert a non-DEFAULT value into column \"g\"",
        ),
        (
            "INSERT INTO vv (id, gg) VALUES (1, 1)",
            "cannot insert a non-DEFAULT value into column \"g\"",
        ),
        (
            "INSERT INTO vr (id, ii) VALUES (1, 1)",
            "cannot insert a non-DEFAULT value into column \"i\"",
        ),
        (
            "INSERT INTO vb SELECT 1, 1, 1",
            "cannot insert a non-DEFAULT value into column \"g\"",
        ),
        (
            "INSERT INTO vr (id, gg) VALUES (1, DEFAULT), (2, 3)",
            "cannot insert a non-DEFAULT value into column \"g\"",
        ),
        (
            "UPDATE vb SET g = 1",
            "column \"g\" can only be updated to DEFAULT",
        ),
        // A view column without a default makes UPDATE's DEFAULT a NULL.
        (
            "UPDATE vb SET g = DEFAULT",
            "column \"g\" can only be updated to DEFAULT",
        ),
        (
            "UPDATE vb SET v = 1",
            "column \"v\" can only be updated to DEFAULT",
        ),
        (
            "UPDATE vb SET i = 1",
            "column \"i\" can only be updated to DEFAULT",
        ),
        (
            "UPDATE vv SET gg = 1",
            "column \"g\" can only be updated to DEFAULT",
        ),
        (
            "MERGE INTO vb USING (SELECT 1 x) s ON true WHEN MATCHED THEN UPDATE SET g = 1",
            "column \"g\" can only be updated to DEFAULT",
        ),
        (
            "MERGE INTO vb USING (SELECT 1 x) s ON false WHEN NOT MATCHED THEN INSERT (id, g) VALUES (1, 1)",
            "cannot insert a non-DEFAULT value into column \"g\"",
        ),
    ] {
        let err = assert_prefix(&db, sql, msg);
        assert!(matches!(err, AnalyzeError::GeneratedAlways(_)), "{err:?}");
    }
    for sql in [
        "INSERT INTO vb (id, a, g) VALUES (1, 1, DEFAULT)",
        "INSERT INTO vb (id, a) VALUES (1, 1)",
        "INSERT INTO vr (id, ii) OVERRIDING SYSTEM VALUE VALUES (1, 1)",
        "INSERT INTO vr (id, gg) VALUES (1, DEFAULT), (2, DEFAULT)",
        "INSERT INTO vr VALUES (1, DEFAULT, DEFAULT)",
        "INSERT INTO vr (id) VALUES (1) RETURNING *",
        "DELETE FROM vr",
    ] {
        assert_ok(&db, sql);
    }
}

/// A view column's own default replaces DEFAULT and missing values, so the
/// base column no longer gets DEFAULT.
#[test]
fn view_column_defaults_reach_the_base_relation() {
    let mut db = setup();
    db.apply_sql("ALTER VIEW vr ALTER COLUMN gg SET DEFAULT 5")
        .unwrap();
    for sql in [
        "INSERT INTO vr (id) VALUES (1)",
        "INSERT INTO vr (id, gg) VALUES (1, DEFAULT)",
    ] {
        let err = assert_prefix(
            &db,
            sql,
            "cannot insert a non-DEFAULT value into column \"g\"",
        );
        assert!(matches!(err, AnalyzeError::GeneratedAlways(_)), "{err:?}");
    }
}

/// view_col_is_auto_updatable: only plain user columns of the base
/// relation can be written.
#[test]
fn non_updatable_view_columns() {
    let db = setup();
    for (sql, msg) in [
        (
            "INSERT INTO vr (a1) VALUES (1)",
            "cannot insert into column \"a1\" of view \"vr\" (View columns that are not columns of their base relation are not updatable.)",
        ),
        (
            "INSERT INTO vr (a1) VALUES (DEFAULT)",
            "cannot insert into column \"a1\" of view \"vr\"",
        ),
        (
            "INSERT INTO vr VALUES (1, DEFAULT, DEFAULT, 3)",
            "cannot insert into column \"a1\" of view \"vr\"",
        ),
        (
            "UPDATE vr SET a1 = 1",
            "cannot update column \"a1\" of view \"vr\"",
        ),
        (
            "UPDATE vr SET a1 = DEFAULT",
            "cannot update column \"a1\" of view \"vr\"",
        ),
        (
            "UPDATE vv SET a1 = 1",
            "cannot update column \"a1\" of view \"vr\"",
        ),
        (
            "UPDATE vs SET c = NULL",
            "cannot update column \"c\" of view \"vs\" (View columns that refer to system columns are not updatable.)",
        ),
        (
            "UPDATE vs SET w = NULL",
            "cannot update column \"w\" of view \"vs\" (View columns that return whole-row references are not updatable.)",
        ),
        (
            "MERGE INTO vr USING (SELECT 1 x) s ON true WHEN MATCHED THEN UPDATE SET a1 = 1",
            "cannot merge into column \"a1\" of view \"vr\"",
        ),
        (
            "MERGE INTO vr USING (SELECT 1 x) s ON false WHEN NOT MATCHED THEN INSERT (a1) VALUES (1)",
            "cannot merge into column \"a1\" of view \"vr\"",
        ),
    ] {
        let err = assert_prefix(&db, sql, msg);
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{err:?}"
        );
    }
    for sql in [
        "MERGE INTO vr USING (SELECT 1 x) s ON false WHEN NOT MATCHED THEN INSERT VALUES (1)",
        "DELETE FROM vx",
        "UPDATE vs SET id = 1",
    ] {
        assert_ok(&db, sql);
    }
}

/// view_query_is_auto_updatable / error_view_not_updatable.
#[test]
fn views_that_are_not_automatically_updatable() {
    let db = setup();
    for (sql, msg) in [
        (
            "INSERT INTO vg VALUES (1, 1)",
            "cannot insert into view \"vg\" (Views containing GROUP BY are not automatically updatable.)",
        ),
        (
            "UPDATE vg SET a = 1",
            "cannot update view \"vg\" (Views containing GROUP BY are not automatically updatable.)",
        ),
        ("DELETE FROM vg", "cannot delete from view \"vg\""),
        (
            "MERGE INTO vg USING (SELECT 1 x) s ON true WHEN MATCHED THEN DELETE",
            "cannot delete from view \"vg\"",
        ),
        (
            "DELETE FROM vt",
            "cannot delete from view \"vt\" (Views containing TABLESAMPLE are not automatically updatable.)",
        ),
        (
            "INSERT INTO vx VALUES (1)",
            "cannot insert into view \"vx\" (Views that have no updatable columns are not automatically updatable.)",
        ),
    ] {
        let err = assert_prefix(&db, sql, msg);
        assert!(
            matches!(err, AnalyzeError::ObjectNotInPrerequisiteState(_)),
            "{err:?}"
        );
    }
}

/// INSTEAD OF triggers take over the view's update; MERGE needs one for
/// every action or none.
#[test]
fn instead_of_triggers() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TRIGGER t1 INSTEAD OF UPDATE ON vr FOR EACH ROW EXECUTE FUNCTION tf();
         CREATE TRIGGER t2 BEFORE INSERT ON vg FOR EACH STATEMENT EXECUTE FUNCTION tf();
         CREATE TRIGGER t3 INSTEAD OF DELETE ON vg FOR EACH ROW EXECUTE FUNCTION tf();",
    )
    .unwrap();
    for sql in [
        "UPDATE vr SET a1 = 1, gg = 3",
        "DELETE FROM vg",
        "MERGE INTO vr USING (SELECT 1 x) s ON true WHEN MATCHED THEN UPDATE SET a1 = 1",
        "MERGE INTO vg USING (SELECT 1 x) s ON true WHEN MATCHED THEN DELETE",
    ] {
        assert_ok(&db, sql);
    }
    let err = assert_prefix(
        &db,
        "INSERT INTO vg VALUES (1, 1)",
        "cannot insert into view \"vg\"",
    );
    assert!(
        matches!(err, AnalyzeError::ObjectNotInPrerequisiteState(_)),
        "{err:?}"
    );
    let err = assert_prefix(
        &db,
        "MERGE INTO vr USING (SELECT 1 x) s ON true WHEN MATCHED THEN UPDATE SET id = 1 \
         WHEN NOT MATCHED THEN INSERT (id) VALUES (1)",
        "cannot merge into view \"vr\" (MERGE is not supported for views with INSTEAD OF triggers for some actions but not all.)",
    );
    assert!(
        matches!(err, AnalyzeError::FeatureNotSupported(_)),
        "{err:?}"
    );
    let err = assert_prefix(
        &db,
        "MERGE INTO vr USING (SELECT 1 x) s ON true WHEN MATCHED THEN UPDATE SET a1 = 1 \
         WHEN NOT MATCHED THEN INSERT (a1) VALUES (1)",
        "cannot merge into column \"a1\" of view \"vr\"",
    );
    assert!(
        matches!(err, AnalyzeError::FeatureNotSupported(_)),
        "{err:?}"
    );
}

/// Rules: an unconditional DO INSTEAD rule replaces the statement, a
/// conditional one blocks the automatic update, MERGE refuses any rule,
/// ON CONFLICT refuses INSERT / UPDATE rules, and RETURNING needs the
/// INSTEAD rule to return something.
#[test]
fn rules_on_the_result_relation() {
    let mut db = setup();
    db.apply_sql(
        "CREATE RULE r1 AS ON INSERT TO vg DO ALSO SELECT 1;
         CREATE RULE r3 AS ON INSERT TO vr DO INSTEAD NOTHING;
         CREATE TABLE c (id int PRIMARY KEY, a int);
         CREATE VIEW vc AS SELECT id, a FROM c;
         CREATE RULE rr AS ON UPDATE TO c DO ALSO SELECT 1;
         CREATE TABLE d (id int);
         CREATE RULE rd AS ON DELETE TO d DO INSTEAD NOTHING;",
    )
    .unwrap();
    let err = assert_prefix(
        &db,
        "INSERT INTO vg VALUES (1, 1)",
        "cannot insert into view \"vg\" (Views containing GROUP BY are not automatically updatable.)",
    );
    assert!(
        matches!(err, AnalyzeError::ObjectNotInPrerequisiteState(_)),
        "{err:?}"
    );
    for sql in [
        "INSERT INTO vr (a1) VALUES (1)",
        "INSERT INTO vr (gg) VALUES (1)",
        "DELETE FROM d",
    ] {
        assert_ok(&db, sql);
    }
    for (sql, msg) in [
        (
            "UPDATE vr SET a1 = 1",
            "cannot update column \"a1\" of view \"vr\"",
        ),
        (
            "MERGE INTO vr USING (SELECT 1 x) s ON true WHEN MATCHED THEN DELETE",
            "cannot execute MERGE on relation \"vr\" (MERGE is not supported for relations with rules.)",
        ),
        (
            "MERGE INTO c USING (SELECT 1 x) s ON true WHEN MATCHED THEN DELETE",
            "cannot execute MERGE on relation \"c\"",
        ),
        (
            "INSERT INTO vc VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = 2",
            "INSERT with ON CONFLICT clause cannot be used with table that has INSERT or UPDATE rules",
        ),
        (
            "INSERT INTO c VALUES (1, 1) ON CONFLICT DO NOTHING",
            "INSERT with ON CONFLICT clause cannot be used with table that has INSERT or UPDATE rules",
        ),
        (
            "DELETE FROM d RETURNING id",
            "cannot perform DELETE RETURNING on relation \"d\"",
        ),
    ] {
        let err = assert_prefix(&db, sql, msg);
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{err:?}"
        );
    }

    db.apply_sql("CREATE RULE r2 AS ON INSERT TO vg WHERE new.a > 0 DO INSTEAD NOTHING")
        .unwrap();
    let err = assert_prefix(
        &db,
        "INSERT INTO vg VALUES (1, 1)",
        "cannot insert into view \"vg\" (Views with conditional DO INSTEAD rules are not automatically updatable.)",
    );
    assert!(
        matches!(err, AnalyzeError::ObjectNotInPrerequisiteState(_)),
        "{err:?}"
    );

    // A disabled rule doesn't fire.
    db.apply_sql("ALTER TABLE d DISABLE RULE rd").unwrap();
    assert_ok(&db, "DELETE FROM d RETURNING id");
}

/// Through a view, NOT NULL and the ON CONFLICT arbiter are the base
/// relation's (the view's columns have neither); ON CONSTRAINT still names
/// a constraint of the view itself.
#[test]
fn base_relation_constraints_through_views() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE c (id int PRIMARY KEY, a int NOT NULL, t text);
         CREATE VIEW vc AS SELECT id AS vid, a AS va, t FROM c;
         CREATE UNIQUE INDEX ON c (lower(t));",
    )
    .unwrap();
    assert_prefix(
        &db,
        "INSERT INTO vc VALUES (1, NULL)",
        "null value in column \"a\" of relation \"c\" violates not-null constraint",
    );
    for sql in [
        "INSERT INTO vc VALUES (1, 1) ON CONFLICT (vid) DO UPDATE SET va = 2",
        "INSERT INTO vc VALUES (1, 1) ON CONFLICT (lower(t)) DO NOTHING",
    ] {
        assert_ok(&db, sql);
    }
    assert_prefix(
        &db,
        "INSERT INTO vc VALUES (1, 1) ON CONFLICT (va) DO NOTHING",
        "there is no unique or exclusion constraint matching the ON CONFLICT specification",
    );
    assert_prefix(
        &db,
        "INSERT INTO vc VALUES (1, 1) ON CONFLICT ON CONSTRAINT c_pkey DO NOTHING",
        "constraint \"c_pkey\" for table \"vc\" does not exist",
    );
}

#[test]
fn rule_actions_are_rewritten_when_the_rule_fires() {
    // transformRuleStmt leaves the rewriter's checks to the statements that
    // fire the rule: these rules are created, and firing them fails.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE g (a int, b int GENERATED ALWAYS AS (a * 2) STORED);
         CREATE TABLE s (a int);
         CREATE RULE r2 AS ON INSERT TO s DO ALSO INSERT INTO g VALUES (new.a, new.a);
         CREATE TABLE u (a int);
         CREATE VIEW vg AS SELECT a, count(*) AS n FROM u GROUP BY a;
         CREATE TABLE w (a int);
         CREATE RULE r3 AS ON UPDATE TO w DO ALSO DELETE FROM vg;
         CREATE TABLE ok (a int);
         CREATE TABLE log (a int);
         CREATE RULE r4 AS ON INSERT TO ok DO ALSO INSERT INTO log VALUES (new.a);
         CREATE TABLE quiet (a int PRIMARY KEY);
         CREATE RULE r5 AS ON INSERT TO quiet DO INSTEAD NOTHING;",
    )
    .unwrap();
    assert_prefix(
        &db,
        "INSERT INTO s VALUES (1)",
        "cannot insert a non-DEFAULT value into column \"b\"",
    );
    assert_prefix(&db, "UPDATE w SET a = 1", "cannot delete from view \"vg\"");
    assert_ok(&db, "INSERT INTO ok VALUES (1)");
    assert_ok(&db, "DELETE FROM w");
    // DO INSTEAD NOTHING yields no product query.
    assert_ok(&db, "INSERT INTO quiet VALUES (1) ON CONFLICT DO NOTHING");
}

#[test]
fn rules_that_fire_themselves_are_infinite_recursion() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (a int);
         CREATE RULE r1 AS ON INSERT TO t DO ALSO INSERT INTO t VALUES (new.a);
         CREATE TABLE p (a int);
         CREATE TABLE q (a int);
         CREATE RULE rp AS ON DELETE TO p DO ALSO DELETE FROM q;
         CREATE RULE rq AS ON DELETE TO q DO ALSO DELETE FROM p;
         CREATE TABLE base (a int);
         CREATE VIEW vbase AS SELECT a FROM base;
         CREATE RULE rb AS ON UPDATE TO base DO ALSO UPDATE vbase SET a = new.a;",
    )
    .unwrap();
    for (sql, relation) in [
        ("INSERT INTO t VALUES (1)", "t"),
        ("DELETE FROM p", "p"),
        ("DELETE FROM q", "q"),
        ("UPDATE base SET a = 1", "base"),
        ("UPDATE vbase SET a = 1", "vbase"),
    ] {
        let err = assert_prefix(
            &db,
            sql,
            &format!("infinite recursion detected in rules for relation \"{relation}\""),
        );
        assert!(
            matches!(err, AnalyzeError::InvalidObjectDefinition(_)),
            "{sql}"
        );
    }
}

#[test]
fn statements_the_executor_always_refuses_are_rejected() {
    // CheckValidResultRel (ExecInitModifyTable): a materialized view or a
    // sequence can't be changed — PREPARE succeeds, every execution fails,
    // whatever the rows; typedpg reports it at compile time.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (a int);
         CREATE MATERIALIZED VIEW mv AS SELECT a FROM t;
         CREATE SEQUENCE s;
         CREATE TYPE ct AS (x int);
         CREATE INDEX ti ON t (a);
         CREATE TABLE logged (a int);
         CREATE RULE rl AS ON INSERT TO logged DO ALSO DELETE FROM mv;",
    )
    .unwrap();
    for (sql, message) in [
        (
            "INSERT INTO mv VALUES (1)",
            "cannot change materialized view \"mv\"",
        ),
        (
            "UPDATE mv SET a = 1",
            "cannot change materialized view \"mv\"",
        ),
        ("DELETE FROM mv", "cannot change materialized view \"mv\""),
        (
            "INSERT INTO mv SELECT a FROM t WHERE false RETURNING a",
            "cannot change materialized view \"mv\"",
        ),
        (
            "WITH x AS (DELETE FROM mv RETURNING a) SELECT * FROM x",
            "cannot change materialized view \"mv\"",
        ),
        (
            "INSERT INTO logged VALUES (1)",
            "cannot change materialized view \"mv\"",
        ),
        (
            "INSERT INTO s VALUES (1, 1, false)",
            "cannot change sequence \"s\"",
        ),
        (
            "UPDATE s SET last_value = 1",
            "cannot change sequence \"s\"",
        ),
        ("DELETE FROM s", "cannot change sequence \"s\""),
        (
            "MERGE INTO mv USING t ON true WHEN MATCHED THEN DELETE",
            "cannot execute MERGE on relation \"mv\"",
        ),
        (
            "MERGE INTO s USING t ON true WHEN MATCHED THEN DELETE",
            "cannot execute MERGE on relation \"s\"",
        ),
        ("INSERT INTO ct VALUES (1)", "cannot open relation \"ct\""),
        ("INSERT INTO ti VALUES (1)", "cannot open relation \"ti\""),
        ("DELETE FROM ti", "cannot open relation \"ti\""),
        ("SELECT * FROM ti", "cannot open relation \"ti\""),
        ("SELECT * FROM ct", "cannot open relation \"ct\""),
    ] {
        let err = assert_prefix(&db, sql, message);
        assert!(
            matches!(
                err,
                AnalyzeError::WrongObjectType(_) | AnalyzeError::FeatureNotSupported(_)
            ),
            "{sql}: {err:?}"
        );
    }
}
