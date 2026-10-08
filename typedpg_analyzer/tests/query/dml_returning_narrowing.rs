//! What RETURNING knows of the rows a data-modifying statement writes.
//!
//! Without a BEFORE ROW trigger, a rule or an INSTEAD OF trigger, an
//! INSERT stores its values and the column defaults, an UPDATE the old row
//! with its SET values, ON CONFLICT DO UPDATE either the inserted row or
//! the updated one, and MERGE one row per action it runs. Generated
//! columns are their expression over the row; every row a statement
//! returns satisfies the table's CHECK constraints; an automatically
//! updatable view writes its base table's rows. Expectations were checked
//! on a live PostgreSQL 18 (and the pg_sanity oracle runs each query over
//! adversarial rows).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, a int,
             b text DEFAULT 'x', ts timestamptz DEFAULT now(), c int NOT NULL DEFAULT 0,
             n text DEFAULT NULL);
         CREATE TABLE src (id int PRIMARY KEY, v int NOT NULL, w int);
         CREATE TABLE tgt (id int PRIMARY KEY, v int, w int, x text DEFAULT 'd');
         CREATE VIEW tv AS SELECT id, v, w FROM tgt;
         CREATE VIEW tv_expr AS SELECT id, v + 0 AS v FROM tgt;
         CREATE TABLE g (id int PRIMARY KEY, c int NOT NULL, a int, w int,
             gs int GENERATED ALWAYS AS (c + 1) STORED,
             gv int GENERATED ALWAYS AS (c * 2) VIRTUAL,
             gc text GENERATED ALWAYS AS (coalesce(a::text, 'none')) STORED,
             gvc text GENERATED ALWAYS AS (coalesce(a::text, 'none')) VIRTUAL,
             ga int GENERATED ALWAYS AS (a + 1) STORED);
         CREATE TABLE u (id serial PRIMARY KEY, email text UNIQUE, n int NOT NULL DEFAULT 0,
             note text);
         CREATE TABLE c6 (id int PRIMARY KEY, kind text NOT NULL, a_id int, b_id int,
             CHECK (kind <> 'a' OR a_id IS NOT NULL));
         CREATE VIEW vk AS SELECT id, kind, a_id FROM c6;
         CREATE VIEW vk_renamed AS SELECT id, kind AS k, a_id AS x FROM c6;
         CREATE TABLE p8 (id int PRIMARY KEY, active boolean NOT NULL, x int,
             CHECK (NOT active OR x IS NOT NULL));
         CREATE TABLE trg (id int PRIMARY KEY, a int, b text DEFAULT 'x');
         CREATE FUNCTION wipe() RETURNS trigger LANGUAGE plpgsql AS
             $$BEGIN NEW.a := NULL; NEW.b := NULL; RETURN NEW; END$$;
         CREATE TRIGGER wipe_ins BEFORE INSERT ON trg FOR EACH ROW EXECUTE FUNCTION wipe();
         CREATE TRIGGER wipe_upd BEFORE UPDATE ON trg FOR EACH ROW EXECUTE FUNCTION wipe();
         CREATE TABLE pt (id int, a int, b text DEFAULT 'x') PARTITION BY LIST (id);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES IN (1, 2);
         CREATE TABLE pt_trg (id int, a int) PARTITION BY LIST (id);
         CREATE TABLE pt_trg1 PARTITION OF pt_trg FOR VALUES IN (1, 2);
         CREATE FUNCTION wipe_a() RETURNS trigger LANGUAGE plpgsql AS
             $$BEGIN NEW.a := NULL; RETURN NEW; END$$;
         CREATE TRIGGER wipe_part BEFORE INSERT ON pt_trg1
             FOR EACH ROW EXECUTE FUNCTION wipe_a();
         CREATE TABLE w (id int PRIMARY KEY, k int NOT NULL);
         CREATE TABLE skip_w (id int PRIMARY KEY, k int NOT NULL);
         CREATE FUNCTION skip_row() RETURNS trigger LANGUAGE plpgsql AS
             $$BEGIN RETURN NULL; END$$;
         CREATE TRIGGER skip BEFORE INSERT ON skip_w FOR EACH ROW EXECUTE FUNCTION skip_row();",
    )
    .unwrap();
    db
}

#[track_caller]
fn assert_nullable(db: &PgCatalog, sql: &str, expected: &[(&str, bool)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, bool)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    assert_eq!(actual, expected, "nullability mismatch for `{sql}`");
}

// ── INSERT: the values and the defaults ─────────────────────────────────────

#[test]
fn insert_returns_its_values_and_defaults() {
    let db = setup();
    assert_nullable(
        &db,
        "INSERT INTO t (a) VALUES (1) RETURNING id, a, b, ts, c, n",
        &[
            ("id", false),
            ("a", false),
            ("b", false),
            ("ts", false),
            ("c", false),
            ("n", true),
        ],
    );
    assert_nullable(
        &db,
        "INSERT INTO t (a, b) VALUES (1, 'b') RETURNING a, b",
        &[("a", false), ("b", false)],
    );
    assert_nullable(
        &db,
        "INSERT INTO t DEFAULT VALUES RETURNING a, b, ts",
        &[("a", true), ("b", false), ("ts", false)],
    );
    assert_nullable(
        &db,
        "INSERT INTO t (a, b) VALUES (1, DEFAULT) RETURNING b",
        &[("b", false)],
    );
    // Each row of a VALUES list.
    assert_nullable(
        &db,
        "INSERT INTO t (a, b) VALUES (1, 'a'), (2, NULL) RETURNING a, b",
        &[("a", false), ("b", true)],
    );
    // INSERT … SELECT: the query's columns.
    assert_nullable(
        &db,
        "INSERT INTO t (a) SELECT c FROM t RETURNING a",
        &[("a", false)],
    );
    assert_nullable(
        &db,
        "INSERT INTO t (a) SELECT a FROM t RETURNING a",
        &[("a", true)],
    );
    assert_nullable(
        &db,
        "INSERT INTO tgt (id, v) SELECT id, v FROM src RETURNING v, w, x",
        &[("v", false), ("w", true), ("x", false)],
    );
    assert_nullable(
        &db,
        "INSERT INTO tgt (id, v, x) VALUES (1, 1, DEFAULT) RETURNING x, new.x AS nx",
        &[("x", false), ("nx", false)],
    );
    // A parameter written to a nullable column may be NULL.
    assert_nullable(
        &db,
        "INSERT INTO tgt (id, v) VALUES (1, $1) RETURNING v",
        &[("v", true)],
    );
}

#[test]
fn insert_through_a_view_returns_the_base_row() {
    let db = setup();
    assert_nullable(
        &db,
        "INSERT INTO tv (id, v) VALUES (1, 2) RETURNING v, w",
        &[("v", false), ("w", true)],
    );
    // A view column computed from the base row is not the value given.
    assert_nullable(
        &db,
        "INSERT INTO tv_expr (id) VALUES (1) RETURNING v",
        &[("v", true)],
    );
}

#[test]
fn insert_with_triggers_keeps_only_what_holds_of_any_row() {
    let db = setup();
    // A BEFORE ROW trigger may rewrite the row.
    assert_nullable(
        &db,
        "INSERT INTO trg (id, a) VALUES (1, 1) RETURNING a, b",
        &[("a", true), ("b", true)],
    );
    // Rows routed to a partition run its triggers.
    assert_nullable(
        &db,
        "INSERT INTO pt_trg (id, a) VALUES (1, 1) RETURNING a",
        &[("a", true)],
    );
    assert_nullable(
        &db,
        "INSERT INTO pt (id, a) VALUES (1, 1) RETURNING a, b",
        &[("a", false), ("b", false)],
    );
}

#[test]
fn insert_cte_returns_its_values() {
    let db = setup();
    assert_nullable(
        &db,
        "WITH i AS (INSERT INTO t (a) VALUES (1) RETURNING a, b) SELECT a, b FROM i",
        &[("a", false), ("b", false)],
    );
}

// ── UPDATE: the SET values ──────────────────────────────────────────────────

#[test]
fn update_returns_its_set_values() {
    let db = setup();
    assert_nullable(
        &db,
        "UPDATE g SET a = 5 RETURNING a, ga",
        &[("a", false), ("ga", false)],
    );
    assert_nullable(
        &db,
        "UPDATE g SET a = c RETURNING a, ga",
        &[("a", false), ("ga", false)],
    );
    // The SET value is computed from the old row WHERE saw.
    assert_nullable(
        &db,
        "UPDATE g SET a = a + 1 WHERE a > 0 RETURNING a",
        &[("a", false)],
    );
    assert_nullable(&db, "UPDATE g SET a = a + 1 RETURNING a", &[("a", true)]);
    assert_nullable(
        &db,
        "UPDATE tgt SET v = src.v FROM src WHERE src.id = tgt.id RETURNING tgt.v",
        &[("v", false)],
    );
    assert_nullable(
        &db,
        "UPDATE tgt SET (v, w) = (1, 2) RETURNING v, w",
        &[("v", false), ("w", false)],
    );
    // A sub-SELECT may return no row.
    assert_nullable(
        &db,
        "UPDATE tgt SET (v, w) = (SELECT 1, 2) RETURNING v, w",
        &[("v", true), ("w", true)],
    );
    assert_nullable(
        &db,
        "UPDATE tgt SET x = DEFAULT RETURNING x",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "WITH u AS (UPDATE g SET a = 1 RETURNING a) SELECT a FROM u",
        &[("a", false)],
    );
    // OLD is the row before the SET.
    assert_nullable(
        &db,
        "UPDATE g SET a = 1 RETURNING old.a AS oa, new.a AS na",
        &[("oa", true), ("na", false)],
    );
    // A BEFORE ROW trigger may rewrite the new row.
    assert_nullable(&db, "UPDATE trg SET a = 1 RETURNING a", &[("a", true)]);
}

#[test]
fn update_set_to_null_drops_what_where_said() {
    let db = setup();
    // The WHERE's disjunction was about the old row.
    assert_nullable(
        &db,
        "UPDATE tgt SET v = NULL WHERE v IS NOT NULL OR w IS NOT NULL RETURNING coalesce(v, w) AS cw",
        &[("cw", true)],
    );
    assert_nullable(
        &db,
        "UPDATE tgt SET x = 'a' WHERE v IS NOT NULL OR w IS NOT NULL RETURNING coalesce(v, w) AS cw",
        &[("cw", false)],
    );
}

#[test]
fn update_through_a_view_keeps_where_facts() {
    let db = setup();
    assert_nullable(
        &db,
        "UPDATE tv SET w = 1 WHERE v IS NOT NULL RETURNING v, w",
        &[("v", false), ("w", false)],
    );
    // The computed column is evaluated again over the new row.
    assert_nullable(
        &db,
        "UPDATE tv_expr SET id = 1 WHERE v IS NOT NULL RETURNING v",
        &[("v", true)],
    );
}

// ── Generated columns ───────────────────────────────────────────────────────

#[test]
fn generated_columns_are_their_expression() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT gs, gv, gc, gvc, ga FROM g",
        &[
            ("gs", false),
            ("gv", false),
            ("gc", false),
            ("gvc", false),
            ("ga", true),
        ],
    );
    // An outer join still null-extends them.
    assert_nullable(
        &db,
        "SELECT g.gs FROM src LEFT JOIN g ON g.id = src.id",
        &[("gs", true)],
    );
    assert_nullable(
        &db,
        "UPDATE g SET w = 1 WHERE a IS NOT NULL RETURNING ga, gs",
        &[("ga", false), ("gs", false)],
    );
    assert_nullable(
        &db,
        "UPDATE g SET a = NULL RETURNING ga, gc, old.ga AS oga",
        &[("ga", true), ("gc", false), ("oga", true)],
    );
    assert_nullable(
        &db,
        "INSERT INTO g (id, c, a) VALUES (1, 1, 1) RETURNING ga, gv",
        &[("ga", false), ("gv", false)],
    );
    assert_nullable(
        &db,
        "INSERT INTO g (id, c) VALUES (1, 1) RETURNING ga",
        &[("ga", true)],
    );
}

// ── ON CONFLICT ─────────────────────────────────────────────────────────────

#[test]
fn on_conflict_returns_either_arm() {
    let db = setup();
    assert_nullable(
        &db,
        "INSERT INTO tgt (id, v) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET v = 2 RETURNING v",
        &[("v", false)],
    );
    assert_nullable(
        &db,
        "INSERT INTO tgt (id, v) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET v = excluded.v
         RETURNING v",
        &[("v", false)],
    );
    // The update arm keeps the conflicting row's `x`.
    assert_nullable(
        &db,
        "INSERT INTO tgt (id, v) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET w = 2
         RETURNING v, x",
        &[("v", true), ("x", true)],
    );
    assert_nullable(
        &db,
        "INSERT INTO tgt (id, v) VALUES (1, 1) ON CONFLICT DO NOTHING RETURNING v, x",
        &[("v", false), ("x", false)],
    );
    // The arbiter key equals the proposed one.
    assert_nullable(
        &db,
        "INSERT INTO u (email) VALUES ('a') ON CONFLICT (email) DO UPDATE SET n = u.n + 1
         RETURNING email",
        &[("email", false)],
    );
    assert_nullable(
        &db,
        "INSERT INTO u (email, note) VALUES ('a', 'b') ON CONFLICT (email)
         DO UPDATE SET note = excluded.note RETURNING note",
        &[("note", false)],
    );
    assert_nullable(
        &db,
        "INSERT INTO u (email, note) VALUES ('a', 'b') ON CONFLICT (email)
         DO UPDATE SET n = 1 RETURNING note",
        &[("note", true)],
    );
    // The DO UPDATE WHERE holds for the updated rows.
    assert_nullable(
        &db,
        "INSERT INTO tgt (id, v, w) VALUES (1, 1, 1) ON CONFLICT (id) DO UPDATE SET v = 2
         WHERE tgt.w IS NOT NULL RETURNING w",
        &[("w", false)],
    );
    assert_nullable(
        &db,
        "INSERT INTO trg (id, a) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET a = 2 RETURNING a",
        &[("a", true)],
    );
}

// ── MERGE ───────────────────────────────────────────────────────────────────

#[test]
fn merge_returns_what_each_action_writes() {
    let db = setup();
    assert_nullable(
        &db,
        "MERGE INTO tgt USING src ON tgt.id = src.id
         WHEN MATCHED THEN UPDATE SET v = src.v
         WHEN NOT MATCHED THEN INSERT (id, v) VALUES (src.id, src.v)
         RETURNING tgt.v, tgt.x",
        &[("v", false), ("x", true)],
    );
    assert_nullable(
        &db,
        "MERGE INTO tgt USING src ON tgt.id = src.id
         WHEN MATCHED AND tgt.v IS NOT NULL THEN UPDATE SET w = 1
         RETURNING tgt.v, tgt.w",
        &[("v", false), ("w", false)],
    );
    assert_nullable(
        &db,
        "MERGE INTO tgt USING (SELECT 1 AS k) s ON tgt.id = s.k
         WHEN MATCHED AND tgt.w IS NOT NULL THEN DELETE RETURNING tgt.w",
        &[("w", false)],
    );
    // An arm that doesn't write `v` from a non-NULL value.
    assert_nullable(
        &db,
        "MERGE INTO tgt USING src ON tgt.id = src.id
         WHEN MATCHED THEN UPDATE SET v = src.v
         WHEN NOT MATCHED THEN INSERT (id) VALUES (src.id)
         RETURNING tgt.v",
        &[("v", true)],
    );
    // WHEN conditions on the source hold for its rows.
    assert_nullable(
        &db,
        "MERGE INTO tgt USING src ON tgt.id = src.id
         WHEN NOT MATCHED AND src.w IS NOT NULL THEN INSERT (id, w) VALUES (src.id, src.w)
         RETURNING src.w, tgt.w",
        &[("w", false), ("w", false)],
    );
    assert_nullable(
        &db,
        "MERGE INTO tgt USING src ON tgt.id = src.id
         WHEN NOT MATCHED BY SOURCE AND tgt.w IS NOT NULL THEN DELETE
         RETURNING src.v, tgt.w",
        &[("v", true), ("w", false)],
    );
    assert_nullable(
        &db,
        "MERGE INTO trg USING src ON trg.id = src.id
         WHEN MATCHED THEN UPDATE SET a = 1 RETURNING trg.a",
        &[("a", true)],
    );
}

// ── CHECK constraints ───────────────────────────────────────────────────────

#[test]
fn returned_rows_satisfy_check_constraints() {
    let db = setup();
    assert_nullable(
        &db,
        "DELETE FROM c6 WHERE kind = 'a' RETURNING a_id",
        &[("a_id", false)],
    );
    assert_nullable(
        &db,
        "DELETE FROM c6 WHERE kind = 'b' RETURNING a_id",
        &[("a_id", true)],
    );
    assert_nullable(
        &db,
        "UPDATE c6 SET b_id = 1 WHERE kind = 'a' RETURNING a_id, old.a_id AS oa",
        &[("a_id", false), ("oa", false)],
    );
    assert_nullable(
        &db,
        "UPDATE c6 SET kind = 'a' WHERE id = 1 RETURNING a_id, old.a_id AS oa",
        &[("a_id", false), ("oa", true)],
    );
    assert_nullable(
        &db,
        "UPDATE c6 SET kind = 'b' WHERE kind = 'a' RETURNING a_id",
        &[("a_id", true)],
    );
    assert_nullable(
        &db,
        "INSERT INTO c6 (id, kind, a_id) VALUES (1, 'a', $1) RETURNING a_id",
        &[("a_id", false)],
    );
    assert_nullable(
        &db,
        "UPDATE p8 SET active = true RETURNING x",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "WITH d AS (DELETE FROM c6 WHERE kind = 'a' RETURNING a_id) SELECT a_id FROM d",
        &[("a_id", false)],
    );
}

#[test]
fn views_read_their_base_table_check_constraints() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT a_id FROM vk WHERE kind = 'a'",
        &[("a_id", false)],
    );
    assert_nullable(
        &db,
        "SELECT x FROM vk_renamed WHERE k = 'a'",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "SELECT a_id FROM vk WHERE kind = 'b'",
        &[("a_id", true)],
    );
}

// ── Data-modifying CTEs returning one row ───────────────────────────────────

#[test]
fn one_row_insert_cte() {
    let db = setup();
    assert_nullable(
        &db,
        "WITH ins AS (INSERT INTO w VALUES (1, 1) RETURNING id) SELECT (SELECT id FROM ins) AS id",
        &[("id", false)],
    );
    assert_nullable(
        &db,
        "WITH ins AS (INSERT INTO w VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET k = 2
         RETURNING id) SELECT (SELECT id FROM ins) AS id",
        &[("id", false)],
    );
    // DO NOTHING, a DO UPDATE WHERE, a trigger returning NULL: no row.
    assert_nullable(
        &db,
        "WITH ins AS (INSERT INTO w VALUES (1, 1) ON CONFLICT DO NOTHING RETURNING id)
         SELECT (SELECT id FROM ins) AS id",
        &[("id", true)],
    );
    assert_nullable(
        &db,
        "WITH ins AS (INSERT INTO w VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET k = 2
         WHERE w.k = 2 RETURNING id) SELECT (SELECT id FROM ins) AS id",
        &[("id", true)],
    );
    assert_nullable(
        &db,
        "WITH ins AS (INSERT INTO skip_w VALUES (1, 1) RETURNING id)
         SELECT (SELECT id FROM ins) AS id",
        &[("id", true)],
    );
    // Not a single row.
    assert_nullable(
        &db,
        "WITH ins AS (INSERT INTO w VALUES (1, 1), (2, 2) RETURNING id)
         SELECT (SELECT id FROM ins LIMIT 1) AS id",
        &[("id", true)],
    );
}

// ── The queue claim: a key IN a locking subquery over the same table ────────

fn queue() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE q (id int PRIMARY KEY, c int, m text, claimed timestamptz, slug text,
             u int UNIQUE, p int);
         CREATE UNIQUE INDEX q_p ON q (p) WHERE p > 0;
         CREATE TABLE other (id int PRIMARY KEY, c int);
         CREATE TABLE parent (id int PRIMARY KEY, c int);
         CREATE TABLE child () INHERITS (parent);",
    )
    .unwrap();
    db
}

/// `UPDATE q … WHERE id IN (SELECT id FROM q WHERE c IS NOT NULL … FOR
/// UPDATE SKIP LOCKED)`: the row updated is the row the subquery locked
/// (`id` a key), on whose newest version its WHERE held — what it proves
/// holds for RETURNING, as for the statement's own WHERE.
#[test]
fn a_locked_key_subquery_over_the_same_table_narrows_the_row() {
    let db = queue();
    for lock in [
        "FOR UPDATE SKIP LOCKED",
        "FOR NO KEY UPDATE",
        "FOR SHARE",
        "FOR UPDATE OF q",
    ] {
        assert_nullable(
            &db,
            &format!(
                "UPDATE q SET claimed = now() WHERE id IN (
                   SELECT id FROM q WHERE c IS NOT NULL AND m IS NOT NULL ORDER BY id LIMIT 5 {lock}
                 ) RETURNING c, m, slug"
            ),
            &[("c", false), ("m", false), ("slug", true)],
        );
    }
    // Another unique key, an alias inside, DELETE, a locking SELECT.
    assert_nullable(
        &db,
        "UPDATE q SET claimed = now() WHERE u IN (SELECT x.u FROM q x WHERE x.c > 0 FOR UPDATE)
         RETURNING c",
        &[("c", false)],
    );
    assert_nullable(
        &db,
        "DELETE FROM q WHERE id IN (SELECT id FROM q WHERE c IS NOT NULL FOR UPDATE) RETURNING c",
        &[("c", false)],
    );
    assert_nullable(
        &db,
        "SELECT c FROM q WHERE id IN (SELECT id FROM q WHERE c IS NOT NULL FOR UPDATE) FOR UPDATE",
        &[("c", false)],
    );
    // The SET still decides what it writes.
    assert_nullable(
        &db,
        "UPDATE q SET c = NULL WHERE id IN (SELECT id FROM q WHERE c IS NOT NULL FOR UPDATE)
         RETURNING c",
        &[("c", true)],
    );
}

/// Without each condition, the row tested may not be the row the WHERE
/// held for: no lock (or KEY SHARE, which lets updates through) leaves a
/// re-fetched row free to have changed (so does locking only another
/// table of a join); a column that isn't a key — or a
/// key only partly unique — may match other rows; another table, or one
/// with inheritance children, has other rows; NOT IN and `<>` ANY match
/// other rows by design.
#[test]
fn the_row_is_narrowed_only_when_it_is_the_locked_one() {
    let db = queue();
    for sql in [
        "UPDATE q SET claimed = now() WHERE id IN (SELECT id FROM q WHERE c IS NOT NULL) RETURNING c",
        "UPDATE q SET claimed = now() WHERE id IN (SELECT id FROM q WHERE c IS NOT NULL FOR KEY SHARE) RETURNING c",
        "UPDATE q SET claimed = now() WHERE id IN (SELECT q.id FROM q JOIN other o ON o.id = q.id WHERE q.c IS NOT NULL FOR UPDATE OF o) RETURNING c",
        "UPDATE q SET claimed = now() WHERE slug IN (SELECT slug FROM q WHERE c IS NOT NULL FOR UPDATE) RETURNING c",
        "UPDATE q SET claimed = now() WHERE p IN (SELECT p FROM q WHERE c IS NOT NULL FOR UPDATE) RETURNING c",
        "UPDATE q SET claimed = now() WHERE id IN (SELECT u FROM q WHERE c IS NOT NULL FOR UPDATE) RETURNING c",
        "UPDATE q SET claimed = now() WHERE id IN (SELECT id FROM other WHERE c IS NOT NULL FOR UPDATE) RETURNING c",
        "UPDATE q SET claimed = now() WHERE id <> ALL (SELECT id FROM q WHERE c IS NOT NULL FOR UPDATE) RETURNING c",
        "UPDATE q SET claimed = now() WHERE NOT (id IN (SELECT id FROM q WHERE c IS NOT NULL FOR UPDATE)) RETURNING c",
        "UPDATE q SET claimed = now() WHERE id IN (SELECT max(id) FROM q WHERE c IS NOT NULL) RETURNING c",
    ] {
        assert_nullable(&db, sql, &[("c", true)]);
    }
    assert_nullable(
        &db,
        "UPDATE parent SET c = c WHERE id IN (SELECT id FROM parent WHERE c IS NOT NULL FOR UPDATE)
         RETURNING c",
        &[("c", true)],
    );
}
