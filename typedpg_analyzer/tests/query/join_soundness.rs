//! Soundness of the row guarantees joins and subqueries narrow by: a
//! set-returning call outside the select list (ORDER BY, GROUP BY,
//! DISTINCT ON) runs in the level's projection like one inside it; a
//! foreign key is followed only by the equality it enforces; a view's
//! stored query reads what it bound when it was defined, not what its names
//! resolve to now; and row locking reaches into the views a statement
//! reads. Each nullable case returns NULL on PostgreSQL 18 over rows the
//! constraints allow; the NOT NULL near misses are checked by the pg_sanity
//! soundness oracle.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE p (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE c (id int PRIMARY KEY, pid int NOT NULL REFERENCES p(id), v text);",
    )
    .unwrap();
    db
}

fn catalog(sql: &str) -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(sql).unwrap();
    db
}

/// The nullability of each of a query's columns (`true` = nullable).
#[track_caller]
fn nullability(db: &PgCatalog, sql: &str) -> Vec<bool> {
    db.analyze(sql)
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .columns
        .iter()
        .map(|c| c.nullable)
        .collect()
}

#[track_caller]
fn assert_not_null(db: &PgCatalog, sqls: &[&str]) {
    for sql in sqls {
        assert_eq!(nullability(db, sql), [false], "`{sql}` should be NOT NULL");
    }
}

#[track_caller]
fn assert_nullable(db: &PgCatalog, sqls: &[&str]) {
    for sql in sqls {
        assert_eq!(nullability(db, sql), [true], "`{sql}` should be nullable");
    }
}

// ── Set-returning calls outside the select list ─────────────────────────────

#[test]
fn a_set_returning_sort_or_grouping_key_can_leave_a_level_without_rows() {
    let db = setup();
    // PG adds the key as a resjunk target entry: its set-returning call
    // runs in the level's projection and, yielding no row, leaves none.
    assert_nullable(
        &db,
        &[
            "SELECT (SELECT 1 ORDER BY generate_series(1, 0)) AS a",
            "SELECT (SELECT 1 GROUP BY generate_series(1, 0)) AS a",
            "SELECT (SELECT count(*) ORDER BY generate_series(1, 0)) AS a",
            "SELECT (SELECT DISTINCT ON (generate_series(1, 0)) 1) AS a",
            "SELECT s.x FROM p LEFT JOIN LATERAL (SELECT 1 AS x ORDER BY generate_series(1, 0)) s ON true",
            "WITH s AS (SELECT 1 AS x ORDER BY generate_series(1, 0)) SELECT (SELECT x FROM s) AS a",
            "SELECT max(s.x) AS a FROM (SELECT 1 AS x ORDER BY generate_series(1, 0)) s",
            // Nor does it keep every row of the table it reads.
            "SELECT pp.name FROM c
                 LEFT JOIN (SELECT * FROM p ORDER BY generate_series(1, 0)) pp ON pp.id = c.pid",
        ],
    );
}

#[test]
fn a_set_returning_sort_or_grouping_key_runs_in_lockstep_with_the_select_list() {
    let db = setup();
    // The shorter of the calls in one projection is padded with NULL.
    assert_nullable(
        &db,
        &[
            "SELECT generate_series(1, 2) AS g ORDER BY generate_series(1, 3)",
            "SELECT unnest(ARRAY[1, 2]) AS g GROUP BY 1, generate_series(1, 3)",
            "SELECT unnest(ARRAY[1, 2, 3]) AS x ORDER BY generate_series(1, 5)",
            "SELECT DISTINCT ON (generate_series(1, 5)) unnest(ARRAY[1, 2, 3]) AS x",
        ],
    );
}

#[test]
fn a_set_returning_sort_key_keeps_a_set_operation_arm_from_always_yielding_null() {
    let db = setup();
    // The right arm yields no row, so it removes no NULL from the left.
    assert_nullable(
        &db,
        &["SELECT c.v FROM c EXCEPT (SELECT NULL ORDER BY generate_series(1, 0))"],
    );
    assert_not_null(&db, &["SELECT c.v FROM c EXCEPT SELECT NULL"]);
}

#[test]
fn a_sort_or_grouping_key_without_a_new_set_returning_call_still_narrows() {
    let db = setup();
    assert_not_null(
        &db,
        &[
            "SELECT (SELECT 1 ORDER BY 1) AS a",
            "SELECT (SELECT count(*) GROUP BY ()) AS a",
            "SELECT s.x FROM p LEFT JOIN LATERAL (SELECT 1 AS x ORDER BY random()) s ON true",
            "SELECT max(s.x) AS a FROM (SELECT 1 AS x ORDER BY 1) s",
            // The select list's own call, named again: one call.
            "SELECT generate_series(1, 3) AS g ORDER BY generate_series(1, 3)",
            "SELECT generate_series(1, 3) AS g GROUP BY generate_series(1, 3)",
            "SELECT generate_series(1, 3) AS g ORDER BY g",
            // Calls yielding as many rows are never padded.
            "SELECT unnest(ARRAY[1, 2]) AS g ORDER BY generate_series(1, 2)",
        ],
    );
}

#[test]
fn a_set_returning_sort_key_forbids_row_locking() {
    let db = setup();
    let err = db
        .analyze("SELECT p.id FROM p ORDER BY generate_series(1, 2) FOR UPDATE")
        .unwrap_err();
    assert!(
        matches!(err, AnalyzeError::FeatureNotSupported(_)),
        "{err:?}"
    );
    assert!(
        err.to_string().starts_with(
            "FOR UPDATE is not allowed with set-returning functions in the target list"
        ),
        "{err}"
    );
}

// ── A VALUES list sorted by a set-returning call ────────────────────────────

#[test]
fn a_values_list_sorted_by_a_set_returning_call_fails_every_execution() {
    let db = setup();
    for sql in [
        "VALUES (1) ORDER BY generate_series(1, 0)",
        "VALUES (1), (2) ORDER BY generate_series(1, 3) LIMIT 0",
        "SELECT (VALUES (1) ORDER BY generate_series(1, 0)) AS a",
        "SELECT ARRAY(VALUES (1) ORDER BY generate_series(1, 0)) AS a FROM p WHERE false",
        "SELECT (SELECT (VALUES (1) ORDER BY generate_series(1, 0)) FROM p WHERE false) AS a",
        "(VALUES (1) ORDER BY generate_series(1, 0)) UNION ALL SELECT 2",
        "INSERT INTO c (id, pid) VALUES (1, 1) ORDER BY generate_series(1, 2)",
        "EXPLAIN VALUES (1) ORDER BY generate_series(1, 0)",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("set-valued function called in context that cannot accept a set"),
            "{sql}: {err}"
        );
    }
}

#[test]
fn a_values_list_sorted_by_a_set_returning_call_the_planner_drops_runs() {
    let db = setup();
    // Constant folding, an unreferenced CTE or EXISTS drop the VALUES
    // before anything initializes it.
    for sql in [
        "SELECT 1 AS a FROM (VALUES (1) ORDER BY generate_series(1, 0)) s WHERE false",
        "WITH s AS (VALUES (1) ORDER BY generate_series(1, 0)) SELECT 1 AS a",
        "SELECT EXISTS (VALUES (1) ORDER BY generate_series(1, 0)) AS a",
        "SELECT CASE WHEN false THEN (VALUES (1) ORDER BY generate_series(1, 0)) END AS a",
        "VALUES (1) ORDER BY 1",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

// ── A foreign key's own equality ─────────────────────────────────────────────

#[test]
fn a_foreign_key_is_not_followed_by_another_equality_than_its_own() {
    // The key compares with `bpchareq` (the text cast to char(3), trailing
    // blanks ignored); the query's `=` resolves to `text = text`.
    let db = catalog(
        "CREATE TABLE pb (k char(3) PRIMARY KEY, name text NOT NULL);
         CREATE TABLE cb (id int PRIMARY KEY, k text NOT NULL REFERENCES pb(k));",
    );
    assert_nullable(
        &db,
        &[
            "SELECT pb.name FROM cb LEFT JOIN pb ON pb.k = cb.k",
            "SELECT pb.name FROM cb LEFT JOIN pb ON cb.k = pb.k",
            "SELECT pb.name FROM cb LEFT JOIN pb USING (k)",
            "SELECT (SELECT pb.name FROM pb WHERE pb.k = cb.k) AS a FROM cb",
        ],
    );
    // A domain's own `=` isn't the btree equality the key uses.
    let db = catalog(
        "CREATE DOMAIN dint AS int;
         CREATE FUNCTION dlt(dint, dint) RETURNS bool LANGUAGE internal IMMUTABLE STRICT
             AS 'int4lt';
         CREATE OPERATOR = (LEFTARG = dint, RIGHTARG = dint, FUNCTION = dlt);
         CREATE TABLE pd (k dint PRIMARY KEY, name text NOT NULL);
         CREATE TABLE cd (id int PRIMARY KEY, k dint NOT NULL REFERENCES pd);",
    );
    assert_nullable(
        &db,
        &[
            "SELECT pd.name FROM cd LEFT JOIN pd ON pd.k = cd.k",
            "SELECT (SELECT pd.name FROM pd WHERE pd.k = cd.k) AS a FROM cd",
        ],
    );
}

#[test]
fn a_foreign_key_whose_equality_depends_on_the_time_zone_is_not_followed() {
    // `timestamptz = timestamp` is STABLE: rows checked under one TimeZone
    // need not match under another.
    let db = catalog(
        "CREATE TABLE pt (ts timestamptz PRIMARY KEY, name text NOT NULL);
         CREATE TABLE ct (id int PRIMARY KEY, ts timestamp NOT NULL REFERENCES pt(ts));",
    );
    assert_nullable(
        &db,
        &[
            "SELECT pt.name FROM ct LEFT JOIN pt ON pt.ts = ct.ts",
            "SELECT (SELECT pt.name FROM pt WHERE pt.ts = ct.ts) AS a FROM ct",
        ],
    );
}

#[test]
fn a_foreign_key_is_followed_by_its_own_equality_across_types() {
    let db = catalog(
        "CREATE TABLE p (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE c8 (id int PRIMARY KEY, pid bigint NOT NULL REFERENCES p(id));
         CREATE DOMAIN did AS int;
         CREATE TABLE cdom (id int PRIMARY KEY, pid did NOT NULL REFERENCES p(id));
         CREATE TABLE pv (k varchar(10) PRIMARY KEY, name text NOT NULL);
         CREATE TABLE cv (id int PRIMARY KEY, k text NOT NULL REFERENCES pv(k));
         CREATE TABLE pts (ts timestamp PRIMARY KEY, name text NOT NULL);
         CREATE TABLE cdate (id int PRIMARY KEY, d date NOT NULL REFERENCES pts(ts));
         CREATE TABLE pb (k char(3) PRIMARY KEY, name text NOT NULL);
         CREATE TABLE cbc (id int PRIMARY KEY, k char(3) NOT NULL REFERENCES pb(k));",
    );
    assert_not_null(
        &db,
        &[
            // int4 = int8 (`int48eq`), written either way round.
            "SELECT p.name FROM c8 LEFT JOIN p ON p.id = c8.pid",
            "SELECT p.name FROM c8 LEFT JOIN p ON c8.pid = p.id",
            "SELECT (SELECT p.name FROM p WHERE p.id = c8.pid) AS a FROM c8",
            "SELECT (SELECT p.name FROM p WHERE c8.pid = p.id) AS a FROM c8",
            // A domain over the key's type compares as the type.
            "SELECT p.name FROM cdom LEFT JOIN p ON p.id = cdom.pid",
            // varchar compares as text.
            "SELECT pv.name FROM cv LEFT JOIN pv ON pv.k = cv.k",
            "SELECT pv.name FROM cv LEFT JOIN pv USING (k)",
            // timestamp = date is immutable.
            "SELECT pts.name FROM cdate LEFT JOIN pts ON pts.ts = cdate.d",
            // char(n) on both sides.
            "SELECT pb.name FROM cbc LEFT JOIN pb ON pb.k = cbc.k",
            "SELECT (SELECT pb.name FROM pb WHERE pb.k = cbc.k) AS a FROM cbc",
        ],
    );
}

// ── A view reads what it bound ───────────────────────────────────────────────

#[test]
fn a_view_keeps_reading_the_table_it_bound_after_a_rename() {
    // `v` reads the original `p` (now `p_old`, `name` nullable), not the
    // new one its text names.
    let db = catalog(
        "CREATE TABLE p (id int PRIMARY KEY, name text);
         CREATE VIEW v AS SELECT p.id, p.name FROM p;
         ALTER TABLE p RENAME TO p_old;
         CREATE TABLE p (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE c (id int PRIMARY KEY, pid int NOT NULL REFERENCES p(id));",
    );
    assert_nullable(
        &db,
        &[
            "SELECT v.name FROM v WHERE v.id > 0",
            "SELECT v.name FROM c LEFT JOIN v ON v.id = c.pid",
        ],
    );
}

#[test]
fn a_view_keeps_reading_the_table_it_bound_under_another_search_path() {
    let db = catalog(
        "CREATE SCHEMA s;
         CREATE TABLE s.p (id int PRIMARY KEY, name text);
         CREATE TABLE public.p (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE c (id int PRIMARY KEY, pid int NOT NULL REFERENCES public.p(id));
         SET search_path = s;
         CREATE VIEW public.v AS SELECT * FROM p;
         RESET search_path;",
    );
    assert_nullable(
        &db,
        &[
            "SELECT v.name FROM v WHERE v.id > 0",
            "SELECT v.name FROM c LEFT JOIN v ON v.id = c.pid",
        ],
    );
}

#[test]
fn a_view_keeps_reading_the_columns_it_bound_after_they_swap_names() {
    let db = catalog(
        "CREATE TABLE p (id int PRIMARY KEY, a text, b text NOT NULL);
         CREATE VIEW v AS SELECT p.id, p.a AS name FROM p;
         ALTER TABLE p RENAME COLUMN a TO tmp;
         ALTER TABLE p RENAME COLUMN b TO a;
         ALTER TABLE p RENAME COLUMN tmp TO b;",
    );
    assert_nullable(&db, &["SELECT v.name FROM v WHERE v.id > 0"]);
    // `v.k` reads the column that was `alt`, which no key references.
    let db = catalog(
        "CREATE TABLE p (id int PRIMARY KEY, alt int NOT NULL, name text NOT NULL);
         CREATE TABLE c (id int PRIMARY KEY, pid int NOT NULL REFERENCES p(id));
         CREATE VIEW v AS SELECT p.alt AS k, p.name FROM p;
         ALTER TABLE p RENAME COLUMN id TO tmp;
         ALTER TABLE p RENAME COLUMN alt TO id;
         ALTER TABLE p RENAME COLUMN tmp TO alt;",
    );
    assert_nullable(&db, &["SELECT v.name FROM c LEFT JOIN v ON v.k = c.pid"]);
}

#[test]
fn a_view_refreshed_after_a_not_null_change_reads_the_table_it_bound() {
    // Dropping NOT NULL from `p_old` re-derives `v`'s nullability from
    // what it reads — `p_old`, not the new `p` its text names.
    let db = catalog(
        "CREATE TABLE p (id int PRIMARY KEY, name text, x int NOT NULL);
         CREATE VIEW v AS SELECT p.name, p.x FROM p;
         ALTER TABLE p RENAME TO p_old;
         CREATE TABLE p (id int PRIMARY KEY, name text NOT NULL, x int NOT NULL);
         ALTER TABLE p_old ALTER COLUMN x DROP NOT NULL;",
    );
    assert_eq!(nullability(&db, "SELECT name, x FROM v"), [true, true]);
}

#[test]
fn a_view_whose_names_still_resolve_alike_still_narrows() {
    let db = catalog(
        "CREATE TABLE p (id int PRIMARY KEY, name text NOT NULL, other text);
         CREATE TABLE c (id int PRIMARY KEY, pid int NOT NULL REFERENCES p(id));
         CREATE VIEW v AS SELECT p.id, p.name FROM p;
         ALTER TABLE p RENAME COLUMN other TO other2;
         CREATE TABLE p2 (id int PRIMARY KEY, name text);
         CREATE VIEW w AS SELECT p.id, p.name, p.other2 FROM p;
         ALTER TABLE p ALTER COLUMN other2 SET NOT NULL;",
    );
    assert_not_null(
        &db,
        &[
            "SELECT v.name FROM c LEFT JOIN v ON v.id = c.pid",
            "SELECT v.name FROM v WHERE v.id > 0",
            "SELECT other2 FROM w",
        ],
    );
}

// ── Row locking inside a view ────────────────────────────────────────────────

#[test]
fn a_view_locking_rows_makes_the_statement_lock_rows() {
    // A row of `cv` updated concurrently is re-fetched with its new key,
    // whose parent the statement's snapshot need not see.
    let db = catalog(
        "CREATE TABLE p (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE c (id int PRIMARY KEY, pid int NOT NULL REFERENCES p(id));
         CREATE VIEW cv AS SELECT * FROM c FOR UPDATE;
         CREATE VIEW cv2 AS SELECT * FROM cv;",
    );
    assert_nullable(
        &db,
        &[
            "SELECT p.name FROM cv LEFT JOIN p ON p.id = cv.pid",
            "SELECT (SELECT p.name FROM p WHERE p.id = cv.pid) AS a FROM cv",
            "SELECT p.name FROM cv2 LEFT JOIN p ON p.id = cv2.pid",
            "SELECT p.name FROM c LEFT JOIN p ON p.id = c.pid FOR UPDATE OF c",
        ],
    );
    assert_not_null(&db, &["SELECT p.name FROM c LEFT JOIN p ON p.id = c.pid"]);
}

#[test]
fn a_statement_locking_rows_does_not_trust_a_view_column_a_foreign_key_narrowed() {
    // `vs.name` is NOT NULL by the foreign key; locked, a concurrently
    // updated `c` row is re-fetched and its subquery run again for the
    // new key, against a snapshot without its parent.
    let db = catalog(
        "CREATE TABLE p (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE c (id int PRIMARY KEY, pid int NOT NULL REFERENCES p(id));
         CREATE VIEW vs AS SELECT c.id, (SELECT p.name FROM p WHERE p.id = c.pid) AS name FROM c;",
    );
    assert_not_null(&db, &["SELECT vs.name FROM vs"]);
    assert_nullable(&db, &["SELECT vs.name FROM vs FOR UPDATE"]);
    assert_not_null(&db, &["SELECT vs.id FROM vs FOR UPDATE"]);
}
