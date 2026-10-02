//! Table inheritance — `CREATE TABLE child INHERITS (parent)`.
//!
//! `pg_inherits` records each (child, parent) edge. Columns from each
//! parent are copied onto the child's `pg_attribute` rows so that a
//! `SELECT FROM child` resolves the same way as PostgreSQL's. DROP
//! COLUMN on a parent is propagated to descendants by walking
//! `pg_inherits`.

use crate::common::*;

#[test]
fn create_table_inherits_copies_columns_into_child() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE animals (
            name  TEXT NOT NULL,
            sound TEXT NOT NULL
         );
         CREATE TABLE dogs (
            breed TEXT NOT NULL
         ) INHERITS (animals);",
    )]);

    // PG (MergeAttributes): inherited columns come first, then the child's
    // own — `\d dogs` on PG 18 lists name, sound, breed.
    let table = snap.resolve_table(Some("public"), "dogs").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let names: Vec<&str> = attrs.iter().map(|a| a.attname.as_str()).collect();
    assert_eq!(names, vec!["name", "sound", "breed"]);
}

#[test]
fn drop_column_on_parent_cascades_to_children() {
    // DROP COLUMN sound CASCADE on the parent removes the inherited
    // column from `dogs` too.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE animals (
            name  TEXT NOT NULL,
            sound TEXT NOT NULL
         );
         CREATE TABLE dogs (
            breed TEXT NOT NULL
         ) INHERITS (animals);",
    )
    .unwrap();
    db.apply_sql("ALTER TABLE animals DROP COLUMN sound CASCADE;")
        .unwrap();

    let dogs = db.resolve_table(Some("public"), "dogs").unwrap();
    let names: Vec<String> = db
        .attributes_of(dogs.oid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    assert_eq!(names, vec!["name".to_string(), "breed".to_string()]);
}

#[test]
fn inherits_propagates_not_null_and_default_flags() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE base (
            id   BIGINT NOT NULL,
            tag  TEXT
         );
         CREATE TABLE leaf () INHERITS (base);",
    )
    .unwrap();
    let leaf = db.resolve_table(Some("public"), "leaf").unwrap();
    let attrs = db.attributes_of(leaf.oid);
    let id = attrs.iter().find(|a| a.attname == "id").unwrap();
    let tag = attrs.iter().find(|a| a.attname == "tag").unwrap();
    assert!(id.attnotnull, "inherited NOT NULL must propagate");
    assert!(!tag.attnotnull, "inherited nullable must propagate");
}

#[test]
fn inherits_multiple_parents_appends_columns_in_order() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE a (x INT NOT NULL);
         CREATE TABLE b (y INT NOT NULL);
         CREATE TABLE c (z INT NOT NULL) INHERITS (a, b);",
    )
    .unwrap();
    let c = db.resolve_table(Some("public"), "c").unwrap();
    let names: Vec<String> = db
        .attributes_of(c.oid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    // Parents' columns in INHERITS order, then the local one.
    assert_eq!(names, vec!["x", "y", "z"]);
}

#[test]
fn inherits_dedupes_same_named_column() {
    // PG: when the child already declares a column with the same name, the
    // parent's copy is merged into it (types must match) — no duplicate row.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE base (id BIGINT NOT NULL, tag TEXT);
         CREATE TABLE leaf (id BIGINT NOT NULL) INHERITS (base);",
    )
    .unwrap();
    let leaf = db.resolve_table(Some("public"), "leaf").unwrap();
    let names: Vec<String> = db
        .attributes_of(leaf.oid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    assert_eq!(names, vec!["id", "tag"]);
}

#[test]
fn drop_column_cascade_descends_two_levels_of_inheritance() {
    // grand <- parent <- child. DROP on grand must also remove the column
    // from `parent` AND `child`.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE grand (id BIGINT NOT NULL, label TEXT);
         CREATE TABLE parent () INHERITS (grand);
         CREATE TABLE child () INHERITS (parent);
         ALTER TABLE grand DROP COLUMN label CASCADE;",
    )
    .unwrap();
    for relname in ["grand", "parent", "child"] {
        let r = db.resolve_table(Some("public"), relname).unwrap();
        let names: Vec<String> = db
            .attributes_of(r.oid)
            .iter()
            .map(|a| a.attname.clone())
            .collect();
        assert_eq!(
            names,
            vec!["id".to_string()],
            "label should be gone from {relname}"
        );
    }
}

#[test]
fn pg_inherits_records_one_row_per_parent() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE a (x INT NOT NULL);
         CREATE TABLE b (y INT NOT NULL);
         CREATE TABLE c () INHERITS (a, b);",
    )
    .unwrap();
    let c = db.resolve_table(Some("public"), "c").unwrap();
    let edges: Vec<i32> = db
        .pg_inherits()
        .iter()
        .filter(|i| i.inhrelid == c.oid)
        .map(|i| i.inhseqno)
        .collect();
    assert_eq!(edges, vec![1, 2], "two parents → seqnos 1 and 2");
}

// ── MergeAttributes: merged columns, partitions ─────────────────────────────

#[test]
fn inherits_puts_inherited_columns_first_and_keeps_parent_not_null() {
    // PG 18 `\d c`: a integer not null, b text, c integer.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int NOT NULL, b text);
         CREATE TABLE c (a int, c int) INHERITS (p);",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM c").unwrap(),
        vec![c("a", int4()), cn("b", text()), cn("c", int4())],
    );
}

#[test]
fn inherits_rejects_a_type_conflict_with_a_local_column() {
    // PG 18: ERROR 42804 column "a" has a type conflict (integer versus text).
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (a int NOT NULL, b text);
             CREATE TABLE c2 (a text) INHERITS (p);",
        )]),
        DdlError::Parse(_),
        "column \"a\" has a type conflict",
    );
    // A typmod difference is a conflict too.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE s (x varchar(5));
             CREATE TABLE c5 (x varchar(6)) INHERITS (s);",
        )]),
        DdlError::Parse(_),
        "column \"x\" has a type conflict",
    );
}

#[test]
fn inherits_merges_columns_of_several_parents() {
    // PG 18 `\d ch3`: a integer not null (merged from p3 and p), z, b, i, c.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int NOT NULL DEFAULT 1, b text, i int);
         CREATE TABLE p3 (a int NULL, z int);
         CREATE TABLE ch3 () INHERITS (p3, p);",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM ch3").unwrap(),
        vec![
            c("a", int4()),
            cn("z", int4()),
            cn("b", text()),
            cn("i", int4()),
        ],
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (a int); CREATE TABLE p2 (a text);
             CREATE TABLE ch2 () INHERITS (p, p2);",
        )]),
        DdlError::Parse(_),
        "inherited column \"a\" has a type conflict",
    );
}

#[test]
fn inherits_rejects_invalid_parents() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE pm (id int) PARTITION BY LIST (id);
             CREATE TABLE c () INHERITS (pm);",
        )]),
        DdlError::Parse(_),
        "cannot inherit from partitioned table \"pm\"",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE VIEW v AS SELECT 1 AS a; CREATE TABLE c2 () INHERITS (v);",
        )]),
        DdlError::Parse(_),
        "inherited relation \"v\" is not a table or foreign table",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE m (id int); CREATE TABLE c3 () INHERITS (m, m);",
        )]),
        DdlError::DuplicateObject(_),
        "relation \"m\" would be inherited from more than once",
    );
}

#[test]
fn partition_of_copies_the_parent_columns() {
    // PG 18: m_2024 has id integer NOT NULL, d date NOT NULL, x text — for
    // RANGE, LIST, HASH and DEFAULT partitions alike.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE m (id int NOT NULL, d date NOT NULL, x text) PARTITION BY RANGE (d);
         CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
         CREATE TABLE l (k text, v int) PARTITION BY LIST (k);
         CREATE TABLE l_def PARTITION OF l DEFAULT;
         CREATE TABLE h (k int) PARTITION BY HASH (k);
         CREATE TABLE h0 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 0);",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM m_2024").unwrap(),
        vec![c("id", int4()), c("d", date()), cn("x", text())],
    );
    assert_cols(
        &db.analyze("SELECT * FROM l_def").unwrap(),
        vec![cn("k", text()), cn("v", int4())],
    );
    assert_cols(
        &db.analyze("SELECT * FROM h0").unwrap(),
        vec![cn("k", int4())],
    );
}

#[test]
fn partition_of_column_options_layer_over_parent_columns() {
    // PG 18 `\d m2`: id, d not null; x text not null default 'a'.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE m (id int NOT NULL, d date NOT NULL, x text) PARTITION BY RANGE (d);
         CREATE TABLE m2 PARTITION OF m (x NOT NULL DEFAULT 'a')
             FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM m2").unwrap(),
        vec![c("id", int4()), c("d", date()), c("x", text())],
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE pm (id int) PARTITION BY LIST (id);
             CREATE TABLE pp2 PARTITION OF pm (zz NOT NULL) FOR VALUES IN (2);",
        )]),
        DdlError::Parse(_),
        "column \"zz\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE m (id int); CREATE TABLE m1 PARTITION OF m FOR VALUES IN (1);",
        )]),
        DdlError::Parse(_),
        "\"m\" is not partitioned",
    );
}

#[test]
fn partition_accepts_inserts_into_parent_columns() {
    // INSERT into a partition directly (it used to have no columns).
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE m (id int NOT NULL, d date NOT NULL, x text) PARTITION BY RANGE (d);
         CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');",
    )]);
    db.analyze("INSERT INTO m_2024 (id, d, x) VALUES (1, '2024-05-01', 'a')")
        .unwrap();
}

// ── ALTER TABLE recursion into children (ATSimpleRecursion & co.) ───────────

#[test]
fn alter_parent_reaches_inheritance_children() {
    // PG 18 `\d c` after the parent's ALTERs: z bigint not null, b text not
    // null default 'x'.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int);
         CREATE TABLE c () INHERITS (p);
         ALTER TABLE p ALTER COLUMN a TYPE bigint;
         ALTER TABLE p ADD COLUMN b text NOT NULL DEFAULT 'x';
         ALTER TABLE p ALTER COLUMN a SET NOT NULL;
         ALTER TABLE p RENAME COLUMN a TO z;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM c").unwrap(),
        vec![c("z", int8()), c("b", text())],
    );
    db.analyze("INSERT INTO c (z) VALUES (1)").unwrap();
}

#[test]
fn alter_parent_reaches_partitions() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE m (id int, d date) PARTITION BY RANGE (d);
         CREATE TABLE m1 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
         ALTER TABLE m ADD COLUMN note text;
         ALTER TABLE m ALTER COLUMN id SET NOT NULL;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM m1").unwrap(),
        // The range bound keeps the key `d` non-NULL.
        vec![c("id", int4()), c("d", date()), cn("note", text())],
    );
}

#[test]
fn drop_not_null_on_parent_keeps_local_child_constraints() {
    // PG 18: c (inherited only) becomes nullable, c2 (also local) stays NOT
    // NULL; DROP NOT NULL on the child itself is refused.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int NOT NULL);
         CREATE TABLE c () INHERITS (p);
         CREATE TABLE c2 (a int NOT NULL) INHERITS (p);
         ALTER TABLE p ALTER a DROP NOT NULL;",
    )]);
    assert_cols(
        &db.analyze("SELECT a FROM p").unwrap(),
        vec![cn("a", int4())],
    );
    assert_cols(
        &db.analyze("SELECT a FROM c").unwrap(),
        vec![cn("a", int4())],
    );
    assert_cols(
        &db.analyze("SELECT a FROM c2").unwrap(),
        vec![c("a", int4())],
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (a int NOT NULL);
             CREATE TABLE c () INHERITS (p);
             ALTER TABLE c ALTER a DROP NOT NULL;",
        )]),
        DdlError::Parse(_),
        "cannot drop inherited constraint \"p_a_not_null\" of relation \"c\"",
    );
}

#[test]
fn add_column_merges_into_a_child_column_of_the_same_type() {
    // PG 18: NOTICE merging definition of column "b" for child "qc";
    // a different type is `child table "rc" has different type for column "b"`.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE q (a int);
         CREATE TABLE qc (b text) INHERITS (q);
         ALTER TABLE q ADD COLUMN b text;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM qc").unwrap(),
        vec![cn("a", int4()), cn("b", text())],
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE r (a int);
             CREATE TABLE rc (b int) INHERITS (r);
             ALTER TABLE r ADD COLUMN b text;",
        )]),
        DdlError::Parse(_),
        "child table \"rc\" has different type for column \"b\"",
    );
}

#[test]
fn inherited_columns_change_only_through_the_parent() {
    // PG 18: 42P16 for every one of these.
    let base = "CREATE TABLE p (z int);
                CREATE TABLE c () INHERITS (p);";
    for (stmt, msg) in [
        (
            "ALTER TABLE ONLY p ADD COLUMN w int;",
            "column must be added to child tables too",
        ),
        (
            "ALTER TABLE ONLY p RENAME COLUMN z TO zz;",
            "inherited column \"z\" must be renamed in child tables too",
        ),
        (
            "ALTER TABLE c RENAME COLUMN z TO zz;",
            "cannot rename inherited column \"z\"",
        ),
        (
            "ALTER TABLE c ALTER COLUMN z TYPE bigint;",
            "cannot alter inherited column \"z\"",
        ),
        (
            "ALTER TABLE c DROP COLUMN z;",
            "cannot drop inherited column \"z\"",
        ),
    ] {
        let result = try_apply(&[("0001.sql", base), ("0002.sql", stmt)]);
        let err = result.expect_err(stmt);
        assert_eq!(err.to_string(), msg, "{stmt}");
    }
}

#[test]
fn drop_column_on_parent_keeps_a_child_local_column() {
    // PG 18: c2 declared `z` itself, so it keeps it; c loses it.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (z int, k int);
         CREATE TABLE c () INHERITS (p);
         CREATE TABLE c2 (z int) INHERITS (p);
         ALTER TABLE p DROP COLUMN z;",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM c").unwrap(),
        vec![cn("k", int4())],
    );
    assert_cols(
        &db.analyze("SELECT * FROM c2").unwrap(),
        vec![cn("z", int4()), cn("k", int4())],
    );
}

#[test]
fn add_column_not_null_is_inherited_under_the_parent_name() {
    // PG 18: c inherits p_b_not_null; c2 keeps its own c2_b_not_null.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int);
         CREATE TABLE c () INHERITS (p);
         CREATE TABLE c2 (b int NOT NULL) INHERITS (p);
         ALTER TABLE p ADD COLUMN b int NOT NULL;",
    )]);
    assert_eq!(
        db.pg_constraint_names_for_table("public", "c"),
        vec!["p_b_not_null"]
    );
    assert_eq!(
        db.pg_constraint_names_for_table("public", "c2"),
        vec!["c2_b_not_null"]
    );
}

#[test]
fn inheritance_respects_temporariness_and_partitions() {
    // MergeAttributes / ATExecAddInherit (PG 18).
    let setup = "CREATE TEMP TABLE tp (a int);
                 CREATE TABLE pp (a int);
                 CREATE TABLE ptab (a int) PARTITION BY LIST (a);
                 CREATE TABLE pt1 PARTITION OF ptab FOR VALUES IN (2);
                 CREATE TEMP TABLE ttab (a int) PARTITION BY LIST (a);
                 CREATE TABLE p1 (a int);";
    for (stmt, msg) in [
        (
            "CREATE TABLE pc () INHERITS (tp);",
            "cannot inherit from temporary relation \"tp\"",
        ),
        (
            "CREATE TEMP TABLE ptemp PARTITION OF ptab FOR VALUES IN (1);",
            "cannot create a temporary relation as partition of permanent relation \"ptab\"",
        ),
        (
            "CREATE TABLE tperm PARTITION OF ttab FOR VALUES IN (1);",
            "cannot create a permanent relation as partition of temporary relation \"ttab\"",
        ),
        (
            "CREATE TABLE ipt () INHERITS (pt1);",
            "cannot inherit from partition \"pt1\"",
        ),
        (
            "ALTER TABLE p1 INHERIT tp;",
            "cannot inherit from temporary relation \"tp\"",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(msg), "{stmt}\n  got: {err}");
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TEMP TABLE tc () INHERITS (tp);
             CREATE TEMP TABLE tc2 () INHERITS (pp);
             CREATE TABLE pg_temp.tx () INHERITS (tp);
             CREATE TEMP TABLE t2 (a int);
             ALTER TABLE t2 INHERIT tp;",
        ),
    ]);
}

#[test]
fn a_column_inherited_from_outside_the_tree_keeps_its_type() {
    // ATPrepAlterColumnType: find_all_inheritors counts each descendant's
    // parents inside the altered tree.
    let err = try_apply(&[(
        "0001.sql",
        "CREATE TABLE parent1 (f1 int);
         CREATE TABLE parent2 (f1 int);
         CREATE TABLE childtab () INHERITS (parent1, parent2);
         ALTER TABLE parent1 ALTER COLUMN f1 TYPE bigint;",
    )])
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot alter inherited column \"f1\" of relation \"childtab\""),
        "{err}"
    );
    build_db(&[(
        "0001.sql",
        "CREATE TABLE top (f1 int);
         CREATE TABLE mid1 () INHERITS (top);
         CREATE TABLE mid2 () INHERITS (top);
         CREATE TABLE bottom () INHERITS (mid1, mid2);
         ALTER TABLE top ALTER COLUMN f1 TYPE bigint;",
    )]);
}

#[test]
fn inherited_expressions_compare_as_parse_trees() {
    // Two spellings of one parse tree are the same CHECK / DEFAULT; a
    // different tree is not.
    build(&[(
        "0001.sql",
        "CREATE TABLE p1 (a int DEFAULT (1 + 2), CONSTRAINT c CHECK (a > 0));
         CREATE TABLE p2 (a int DEFAULT 1+2, CONSTRAINT c CHECK ((a)>0));
         CREATE TABLE child () INHERITS (p1, p2);
         INSERT INTO child DEFAULT VALUES;
         CREATE TABLE local (a int, CONSTRAINT c CHECK ( a  >  0 )) INHERITS (p1);",
    )]);
    let base = "CREATE TABLE p1 (a int DEFAULT 1, CONSTRAINT c CHECK (a > 0));
                CREATE TABLE p2 (a int DEFAULT 1, CONSTRAINT c CHECK (a >= 0));";
    for (sql, message) in [
        (
            "CREATE TABLE child (a int, CONSTRAINT c CHECK (a >= 0)) INHERITS (p1);",
            "constraint \"c\" for relation \"child\" already exists",
        ),
        (
            "CREATE TABLE child () INHERITS (p1, p2);",
            "check constraint name \"c\" appears multiple times but with different expressions",
        ),
        (
            "CREATE TABLE child (a int) INHERITS (p1, p2);",
            "check constraint name \"c\" appears multiple times but with different expressions",
        ),
        (
            "CREATE STATISTICS s ON (a+1), ((a) + 1) FROM p1;",
            "duplicate expression in statistics definition",
        ),
    ] {
        let err = try_apply(&[("0001.sql", base), ("0002.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(message), "{sql}\n  got: {err}");
    }
}
