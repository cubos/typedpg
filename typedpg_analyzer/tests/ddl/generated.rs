//! Generated columns, STORED and PG 18's VIRTUAL (the default kind): what
//! the catalog records and which DDL PG refuses over them. Expected
//! messages are PG 18's.

use crate::common::*;
use typedpg_analyzer::AttGenerated;

/// Apply `setup`, then `stmt`, and return `stmt`'s error message.
fn ddl_err(setup: &str, stmt: &str) -> String {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(setup).unwrap();
    match db.apply_sql(stmt) {
        Ok(()) => panic!("expected {stmt:?} to fail"),
        Err(e) => e.to_string(),
    }
}

fn assert_err(setup: &str, stmt: &str, expected_prefix: &str) {
    let msg = ddl_err(setup, stmt);
    assert!(
        msg.starts_with(expected_prefix),
        "{stmt}\n  expected prefix: {expected_prefix:?}\n  got:             {msg:?}"
    );
}

fn generated_kinds(db: &PgCatalog, table: &str) -> Vec<(String, Option<AttGenerated>)> {
    let t = db.resolve_table(None, table).unwrap();
    db.attributes_of(t.oid)
        .iter()
        .map(|a| (a.attname.clone(), a.attgenerated))
        .collect()
}

// ── What the catalog records ────────────────────────────────────────────────

#[test]
fn virtual_is_the_default_generation_kind() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int,
            b int GENERATED ALWAYS AS (a * 2),
            c int GENERATED ALWAYS AS (a * 3) STORED,
            d int GENERATED ALWAYS AS (a + 1) VIRTUAL);
         ALTER TABLE t ADD COLUMN e int GENERATED ALWAYS AS (a - 1);",
    )]);
    assert_eq!(
        generated_kinds(&db, "t"),
        vec![
            ("a".to_owned(), None),
            ("b".to_owned(), Some(AttGenerated::Virtual)),
            ("c".to_owned(), Some(AttGenerated::Stored)),
            ("d".to_owned(), Some(AttGenerated::Virtual)),
            ("e".to_owned(), Some(AttGenerated::Virtual)),
        ]
    );
}

#[test]
fn virtual_columns_may_be_not_null_and_checked() {
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a) VIRTUAL NOT NULL CHECK (b > 0),
            c text GENERATED ALWAYS AS (a::text) VIRTUAL COLLATE \"C\",
            d int GENERATED ALWAYS AS (tableoid::int) VIRTUAL,
            e int GENERATED ALWAYS AS ((t).a) VIRTUAL);
         ALTER TABLE t ALTER COLUMN b TYPE bigint;
         ALTER TABLE t ALTER COLUMN c TYPE varchar(10);",
    )]);
}

// ── Indexes and index-backed constraints ────────────────────────────────────

const T: &str = "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL,
    c int GENERATED ALWAYS AS (a * 3) STORED);
    CREATE TABLE r (x int PRIMARY KEY);";

#[test]
fn indexes_on_virtual_columns_are_rejected() {
    let msg = "indexes on virtual generated columns are not supported";
    assert_err(T, "CREATE INDEX ON t (b);", msg);
    assert_err(T, "CREATE INDEX ON t ((b + 1));", msg);
    assert_err(T, "CREATE INDEX ON t (a) WHERE b > 0;", msg);
    assert_err(T, "CREATE INDEX ON t (a) INCLUDE (b);", msg);
    build_db(&[("0001.sql", T), ("0002.sql", "CREATE INDEX ON t (c);")]);
}

#[test]
fn indexes_on_system_columns_are_rejected() {
    assert_err(
        T,
        "CREATE INDEX ON t (ctid);",
        "index creation on system columns is not supported",
    );
    assert_err(
        T,
        "CREATE INDEX ON t ((xmin::text));",
        "index creation on system columns is not supported",
    );
}

#[test]
fn key_constraints_on_virtual_columns_are_rejected() {
    let unique = "unique constraints on virtual generated columns are not supported";
    let primary = "primary keys on virtual generated columns are not supported";
    assert_err(T, "ALTER TABLE t ADD CONSTRAINT u UNIQUE (b);", unique);
    assert_err(T, "ALTER TABLE t ADD PRIMARY KEY (b);", primary);
    assert_err(
        T,
        "ALTER TABLE t ADD CONSTRAINT ex EXCLUDE USING btree (b WITH =);",
        unique,
    );
    assert_err(
        "",
        "CREATE TABLE v (a int, b int GENERATED ALWAYS AS (a) VIRTUAL UNIQUE);",
        unique,
    );
    assert_err(
        "",
        "CREATE TABLE v (a int, b int GENERATED ALWAYS AS (a) VIRTUAL PRIMARY KEY);",
        primary,
    );
    assert_err(
        "",
        "CREATE TABLE v (a int, b int GENERATED ALWAYS AS (a) VIRTUAL, PRIMARY KEY (a, b));",
        primary,
    );
    assert_err(
        T,
        "ALTER TABLE t ADD COLUMN e int GENERATED ALWAYS AS (a) VIRTUAL UNIQUE;",
        unique,
    );
    build_db(&[
        ("0001.sql", T),
        ("0002.sql", "ALTER TABLE t ADD UNIQUE (c);"),
    ]);
}

#[test]
fn foreign_keys_over_virtual_columns_are_rejected() {
    let msg = "foreign key constraints on virtual generated columns are not supported";
    assert_err(
        T,
        "ALTER TABLE t ADD FOREIGN KEY (b) REFERENCES r (x);",
        msg,
    );
    assert_err(
        "CREATE TABLE r (x int PRIMARY KEY);",
        "CREATE TABLE v (a int, b int GENERATED ALWAYS AS (a) VIRTUAL REFERENCES r);",
        msg,
    );
}

#[test]
fn foreign_keys_over_generated_columns_restrict_their_actions() {
    let setup = "CREATE TABLE r (x int PRIMARY KEY);";
    for (action, msg) in [
        (
            "ON UPDATE CASCADE",
            "invalid ON UPDATE action for foreign key constraint containing generated column",
        ),
        (
            "ON UPDATE SET NULL",
            "invalid ON UPDATE action for foreign key constraint containing generated column",
        ),
        (
            "ON DELETE SET NULL",
            "invalid ON DELETE action for foreign key constraint containing generated column",
        ),
        (
            "ON DELETE SET DEFAULT",
            "invalid ON DELETE action for foreign key constraint containing generated column",
        ),
    ] {
        assert_err(
            setup,
            &format!(
                "CREATE TABLE v (a int, b int GENERATED ALWAYS AS (a) STORED REFERENCES r {action});"
            ),
            msg,
        );
    }
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE v (a int, b int GENERATED ALWAYS AS (a) STORED REFERENCES r ON DELETE CASCADE);",
        ),
    ]);
}

#[test]
fn statistics_on_virtual_columns_are_rejected() {
    let msg = "statistics creation on virtual generated columns is not supported";
    assert_err(T, "CREATE STATISTICS s1 ON a, b FROM t;", msg);
    assert_err(T, "CREATE STATISTICS s3 ON (b + 1), a FROM t;", msg);
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN b SET STATISTICS 10;",
        "cannot alter statistics on virtual generated column \"b\"",
    );
    build_db(&[
        ("0001.sql", T),
        ("0002.sql", "CREATE STATISTICS s2 ON a, c FROM t;"),
    ]);
}

#[test]
fn publication_column_lists_exclude_virtual_columns() {
    assert_err(
        T,
        "CREATE PUBLICATION p FOR TABLE t (a, b);",
        "cannot use virtual generated column \"b\" in publication column list",
    );
}

// ── The type of a virtual column ────────────────────────────────────────────

#[test]
fn virtual_columns_have_built_in_non_domain_types() {
    let setup = "CREATE DOMAIN dpos AS int CHECK (VALUE > 0);
        CREATE TYPE comp AS (x int);
        CREATE TYPE dposrange AS RANGE (subtype = dpos);
        CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a) VIRTUAL);";
    assert_err(
        setup,
        "CREATE TABLE v (a int, b dpos GENERATED ALWAYS AS (a) VIRTUAL);",
        "virtual generated column \"b\" cannot have a domain type",
    );
    assert_err(
        setup,
        "CREATE TABLE v (a int, b comp GENERATED ALWAYS AS (row(a)::comp) VIRTUAL);",
        "virtual generated column \"b\" cannot have a user-defined type",
    );
    assert_err(
        setup,
        "CREATE TABLE v (a int, b dposrange GENERATED ALWAYS AS (dposrange(a, a + 5)) VIRTUAL);",
        "virtual generated column \"b\" cannot have a domain type",
    );
    assert_err(
        setup,
        "ALTER TABLE t ADD COLUMN d dpos GENERATED ALWAYS AS (a) VIRTUAL;",
        "virtual generated column \"d\" cannot have a domain type",
    );
    assert_err(
        setup,
        "ALTER TABLE t ALTER COLUMN b TYPE dpos;",
        "virtual generated column \"b\" cannot have a domain type",
    );
    // A stored one may.
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE s (a int, b dpos GENERATED ALWAYS AS (a) STORED);",
        ),
    ]);
}

// ── The generation expression ───────────────────────────────────────────────

#[test]
fn virtual_generation_expressions_use_built_in_functions_and_types() {
    let setup = "CREATE FUNCTION gf1(a int) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT a $$;
        CREATE DOMAIN d1 AS int;";
    assert_err(
        setup,
        "CREATE TABLE v (a int, b int GENERATED ALWAYS AS (gf1(a)) VIRTUAL);",
        "generation expression uses user-defined function",
    );
    assert_err(
        setup,
        "CREATE TABLE v (a d1, b d1, c int GENERATED ALWAYS AS (greatest(a, b)) VIRTUAL);",
        "generation expression uses user-defined type",
    );
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE s (a int, b int GENERATED ALWAYS AS (gf1(a)) STORED);",
        ),
    ]);
}

#[test]
fn generation_expressions_are_checked_like_cookdefault() {
    for (kind, _) in [("VIRTUAL", 0), ("STORED", 1)] {
        let create = |expr: &str| {
            format!("CREATE TABLE v (a int, b int GENERATED ALWAYS AS ({expr}) {kind});")
        };
        assert_err(
            "",
            &create("num_nulls(v)"),
            "cannot use whole-row variable in column generation expression",
        );
        assert_err(
            "",
            &create("xmin::text::int"),
            "cannot use system column \"xmin\" in column generation expression",
        );
        assert_err(
            "",
            &create("generate_series(1, a)"),
            "set-returning functions are not allowed in column generation expressions",
        );
        assert_err(
            "",
            &create("(SELECT 1)"),
            "cannot use subquery in column generation expression",
        );
        assert_err(
            "",
            &create("now()::text::int"),
            "generation expression is not immutable",
        );
    }
    assert_err(
        "",
        "CREATE TABLE v (a int, b int GENERATED ALWAYS AS (a) VIRTUAL,
            c int GENERATED ALWAYS AS (b) VIRTUAL);",
        "cannot use generated column \"b\" in column generation expression",
    );
    assert_err(
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a) VIRTUAL);",
        "ALTER TABLE t ADD COLUMN c int GENERATED ALWAYS AS (b + 1) VIRTUAL;",
        "cannot use generated column \"b\" in column generation expression",
    );
    assert_err(
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a) VIRTUAL, x int);",
        "ALTER TABLE t ALTER COLUMN b SET EXPRESSION AS (b * 2);",
        "cannot use generated column \"b\" in column generation expression",
    );
}

#[test]
fn column_clauses_conflict_like_transform_column_definition() {
    assert_err(
        "",
        "CREATE TABLE v (a int PRIMARY KEY, b int GENERATED ALWAYS AS (a * 2) VIRTUAL
            GENERATED ALWAYS AS (a * 3) VIRTUAL);",
        "multiple generation clauses specified for column \"b\" of table \"v\"",
    );
    assert_err(
        "",
        "CREATE TABLE v (a int, b serial GENERATED ALWAYS AS (a) VIRTUAL);",
        "both default and generation expression specified for column \"b\" of table \"v\"",
    );
    assert_err(
        "",
        "CREATE TABLE v (a int DEFAULT 1 DEFAULT 2);",
        "multiple default values specified for column \"a\" of table \"v\"",
    );
    assert_err(
        "CREATE TYPE ty AS (f1 int, f2 int);",
        "CREATE TABLE v OF ty (f1 WITH OPTIONS GENERATED ALWAYS AS (f2 * 2) VIRTUAL);",
        "generated columns are not supported on typed tables",
    );
    assert_err(
        "",
        "CREATE TABLE v (a int GENERATED BY DEFAULT AS (1) VIRTUAL);",
        "for a generated column, GENERATED ALWAYS must be specified",
    );
}

// ── ALTER TABLE over generated columns ──────────────────────────────────────

#[test]
fn drop_expression_is_not_supported_for_virtual_columns() {
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN b DROP EXPRESSION;",
        "ALTER TABLE / DROP EXPRESSION is not supported for virtual generated columns",
    );
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN b DROP EXPRESSION IF EXISTS;",
        "ALTER TABLE / DROP EXPRESSION is not supported for virtual generated columns",
    );
    let db = build_db(&[
        ("0001.sql", T),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN c DROP EXPRESSION;"),
    ]);
    assert_eq!(generated_kinds(&db, "t")[2], ("c".to_owned(), None));
}

#[test]
fn drop_expression_reaches_the_children() {
    let setup = "CREATE TABLE p (a int, b int GENERATED ALWAYS AS (a * 2) STORED);
        CREATE TABLE c () INHERITS (p);";
    assert_err(
        setup,
        "ALTER TABLE ONLY p ALTER COLUMN b DROP EXPRESSION;",
        "ALTER TABLE / DROP EXPRESSION must be applied to child tables too",
    );
    assert_err(
        setup,
        "ALTER TABLE c ALTER COLUMN b DROP EXPRESSION;",
        "cannot drop generation expression from inherited column",
    );
    let db = build_db(&[
        ("0001.sql", setup),
        ("0002.sql", "ALTER TABLE p ALTER COLUMN b DROP EXPRESSION;"),
    ]);
    assert_eq!(generated_kinds(&db, "c")[1], ("b".to_owned(), None));
}

#[test]
fn set_expression_of_a_virtual_column_needs_a_table_without_checks_or_publications() {
    assert_err(
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a) VIRTUAL CHECK (b < 50));",
        "ALTER TABLE t ALTER COLUMN b SET EXPRESSION AS (a * 2);",
        "ALTER TABLE / SET EXPRESSION is not supported for virtual generated columns in \
         tables with check constraints",
    );
    assert_err(
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a) VIRTUAL);
         CREATE PUBLICATION p FOR TABLE t;",
        "ALTER TABLE t ALTER COLUMN b SET EXPRESSION AS (a * 2);",
        "ALTER TABLE / SET EXPRESSION is not supported for virtual generated columns in \
         tables that are part of a publication",
    );
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN xmin SET EXPRESSION AS (1);",
        "cannot alter system column \"xmin\"",
    );
    build_db(&[
        (
            "0001.sql",
            "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a) STORED CHECK (b < 50));",
        ),
        (
            "0002.sql",
            "ALTER TABLE t ALTER COLUMN b SET EXPRESSION AS (a * 2);",
        ),
    ]);
}

#[test]
fn defaults_of_generated_columns_go_through_their_expression() {
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN b SET DEFAULT 1;",
        "column \"b\" of relation \"t\" is a generated column",
    );
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN c DROP DEFAULT;",
        "column \"c\" of relation \"t\" is a generated column",
    );
    assert_err(
        "CREATE TABLE t (a int GENERATED ALWAYS AS IDENTITY);",
        "ALTER TABLE t ALTER COLUMN a DROP DEFAULT;",
        "column \"a\" of relation \"t\" is an identity column",
    );
}

#[test]
fn alter_type_respects_generation_expressions() {
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN b TYPE text USING b::text;",
        "cannot specify USING when altering type of generated column",
    );
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN a TYPE bigint;",
        "cannot alter type of a column used by a generated column",
    );
    // A stored column's values are cast first; a virtual one has none.
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN c TYPE date;",
        "column \"c\" cannot be cast automatically to type date",
    );
    assert_err(
        T,
        "ALTER TABLE t ALTER COLUMN b TYPE date;",
        "generation expression for column \"b\" cannot be cast automatically to type date",
    );
    build_db(&[
        ("0001.sql", T),
        ("0002.sql", "ALTER TABLE t ALTER COLUMN b TYPE bigint;"),
    ]);
}

#[test]
fn drop_column_read_by_a_generation_expression_needs_cascade() {
    let setup = "CREATE TABLE g (a int, b int,
        c int GENERATED ALWAYS AS (b * 2) VIRTUAL,
        d int GENERATED ALWAYS AS (a + 1) STORED);";
    assert_err(
        setup,
        "ALTER TABLE g DROP COLUMN b;",
        "cannot drop column b of table g because other objects depend on it",
    );
    let db = build_db(&[
        ("0001.sql", setup),
        ("0002.sql", "ALTER TABLE g DROP COLUMN b CASCADE;"),
    ]);
    let names: Vec<String> = generated_kinds(&db, "g")
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["a", "d"]);
}

#[test]
fn partition_keys_may_not_read_generated_columns() {
    for kind in ["VIRTUAL", "STORED"] {
        for key in ["b", "(b)", "(b + 1)", "(v)", "(v IS NOT NULL)"] {
            assert_err(
                "",
                &format!(
                    "CREATE TABLE v (a int, b int GENERATED ALWAYS AS (a) {kind})
                     PARTITION BY RANGE ({key});"
                ),
                "cannot use generated column in partition key",
            );
        }
    }
    assert_err(
        "",
        "CREATE TABLE v (a int) PARTITION BY RANGE ((xmin::text));",
        "partition key expressions cannot contain system column references",
    );
}

// ── Inheritance and partitions ──────────────────────────────────────────────

const PARENT: &str = "CREATE TABLE gtest1 (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL);
    CREATE TABLE gtest_normal (a int, b int);";

#[test]
fn a_child_column_is_generated_exactly_when_its_parent_column_is() {
    assert_err(
        PARENT,
        "CREATE TABLE c (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL) INHERITS (gtest_normal);",
        "child column \"b\" specifies generation expression",
    );
    assert_err(
        PARENT,
        "CREATE TABLE c (x int, b int DEFAULT 10) INHERITS (gtest1);",
        "column \"b\" inherits from generated column but specifies default",
    );
    assert_err(
        PARENT,
        "CREATE TABLE c (x int, b int GENERATED ALWAYS AS IDENTITY) INHERITS (gtest1);",
        "column \"b\" inherits from generated column but specifies identity",
    );
    assert_err(
        PARENT,
        "CREATE TABLE c (x int, b int GENERATED ALWAYS AS (a * 22) STORED) INHERITS (gtest1);",
        "column \"b\" inherits from generated column of different kind",
    );
    // The child's own expression of the same kind overrides the parent's.
    let db = build_db(&[
        ("0001.sql", PARENT),
        (
            "0002.sql",
            "CREATE TABLE c (x int, b int GENERATED ALWAYS AS (a * 22) VIRTUAL) INHERITS (gtest1);
             CREATE TABLE d () INHERITS (gtest1);",
        ),
    ]);
    assert_eq!(
        generated_kinds(&db, "d")[1],
        ("b".to_owned(), Some(AttGenerated::Virtual))
    );
}

#[test]
fn alter_inherit_checks_generation_of_each_column() {
    let setup = format!(
        "{PARENT}
         CREATE TABLE gen_child (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL);
         CREATE TABLE plain_child (a int, b int);
         CREATE TABLE stored_child (a int, b int GENERATED ALWAYS AS (a * 2) STORED);"
    );
    assert_err(
        &setup,
        "ALTER TABLE gen_child INHERIT gtest_normal;",
        "column \"b\" in child table must not be a generated column",
    );
    assert_err(
        &setup,
        "ALTER TABLE plain_child INHERIT gtest1;",
        "column \"b\" in child table must be a generated column",
    );
    assert_err(
        &setup,
        "ALTER TABLE stored_child INHERIT gtest1;",
        "column \"b\" inherits from generated column of different kind",
    );
    build_db(&[
        ("0001.sql", &setup),
        ("0002.sql", "ALTER TABLE gen_child INHERIT gtest1;"),
    ]);
}

#[test]
fn several_parents_agree_on_generation_and_defaults() {
    assert_err(
        "CREATE TABLE p1 (a int, b int DEFAULT 10);
         CREATE TABLE p2 (x int, b int GENERATED ALWAYS AS (x * 22) VIRTUAL);",
        "CREATE TABLE c () INHERITS (p1, p2);",
        "inherited column \"b\" has a generation conflict",
    );
    let conflicting = "CREATE TABLE p1 (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL);
         CREATE TABLE p2 (x int, b int GENERATED ALWAYS AS (x * 22) VIRTUAL);";
    assert_err(
        conflicting,
        "CREATE TABLE c () INHERITS (p1, p2);",
        "column \"b\" inherits conflicting generation expressions",
    );
    build_db(&[
        ("0001.sql", conflicting),
        (
            "0002.sql",
            "CREATE TABLE c (b int GENERATED ALWAYS AS (x + 1) VIRTUAL) INHERITS (p1, p2);",
        ),
    ]);
    assert_err(
        "CREATE TABLE p1 (a int DEFAULT 1); CREATE TABLE p2 (a int DEFAULT 2);",
        "CREATE TABLE c () INHERITS (p1, p2);",
        "column \"a\" inherits conflicting default values",
    );
    build_db(&[(
        "0001.sql",
        "CREATE TABLE p1 (a int DEFAULT 1); CREATE TABLE p2 (a int DEFAULT 1);
         CREATE TABLE p3 (a int DEFAULT 2);
         CREATE TABLE c1 () INHERITS (p1, p2);
         CREATE TABLE c2 (a int DEFAULT 3) INHERITS (p1, p3);",
    )]);
}

#[test]
fn partitions_follow_the_parent_generation() {
    let plain = "CREATE TABLE gp (f1 date NOT NULL, f2 bigint, f3 bigint) PARTITION BY RANGE (f1);";
    assert_err(
        plain,
        "CREATE TABLE gc PARTITION OF gp (f3 WITH OPTIONS GENERATED ALWAYS AS (f2 * 2) VIRTUAL)
         FOR VALUES FROM ('2016-07-01') TO ('2016-08-01');",
        "child column \"f3\" specifies generation expression",
    );
    let generated = "CREATE TABLE gp (f1 date NOT NULL, f2 bigint,
        f3 bigint GENERATED ALWAYS AS (f2 * 2) VIRTUAL) PARTITION BY RANGE (f1);";
    let bounds = "FOR VALUES FROM ('2016-09-01') TO ('2016-10-01')";
    assert_err(
        generated,
        &format!("CREATE TABLE gc PARTITION OF gp (f3 DEFAULT 42) {bounds};"),
        "column \"f3\" inherits from generated column but specifies default",
    );
    assert_err(
        generated,
        &format!(
            "CREATE TABLE gc PARTITION OF gp (f3 WITH OPTIONS GENERATED ALWAYS AS IDENTITY) {bounds};"
        ),
        "identity columns are not supported on partitions",
    );
    assert_err(
        generated,
        &format!(
            "CREATE TABLE gc PARTITION OF gp (f3 GENERATED ALWAYS AS (f2 * 2) STORED) {bounds};"
        ),
        "column \"f3\" inherits from generated column of different kind",
    );
    for (f3, msg) in [
        (
            "bigint",
            "column \"f3\" in child table must be a generated column",
        ),
        (
            "bigint GENERATED ALWAYS AS IDENTITY",
            "table \"gc\" being attached contains an identity column \"f3\"",
        ),
        (
            "bigint GENERATED ALWAYS AS (f2 * 33) STORED",
            "column \"f3\" inherits from generated column of different kind",
        ),
    ] {
        assert_err(
            &format!("{generated} CREATE TABLE gc (f1 date NOT NULL, f2 bigint, f3 {f3});"),
            &format!("ALTER TABLE gp ATTACH PARTITION gc {bounds};"),
            msg,
        );
    }
    build_db(&[(
        "0001.sql",
        &format!(
            "{generated}
             CREATE TABLE gc1 PARTITION OF gp FOR VALUES FROM ('2016-07-01') TO ('2016-08-01');
             CREATE TABLE gc2 PARTITION OF gp (f3 WITH OPTIONS GENERATED ALWAYS AS (f2 * 22) VIRTUAL)
               FOR VALUES FROM ('2016-08-01') TO ('2016-09-01');
             CREATE TABLE gc3 (f1 date NOT NULL, f2 bigint,
               f3 bigint GENERATED ALWAYS AS (f2 * 33) VIRTUAL);
             ALTER TABLE gp ATTACH PARTITION gc3 {bounds};"
        ),
    )]);
}

#[test]
fn a_call_leaving_parameters_out_runs_their_defaults() {
    // CheckMutability looks at a call with its DEFAULT arguments filled
    // in: a stable `now()` default makes it mutable — unless the function
    // is inlined (a simple SQL body), which drops an unused argument.
    let setup = "CREATE SEQUENCE s;
         CREATE FUNCTION f(n bigint DEFAULT nextval('s')) RETURNS int LANGUAGE sql IMMUTABLE
             AS 'SELECT 1';
         CREATE FUNCTION h(n timestamptz DEFAULT now()) RETURNS int LANGUAGE plpgsql IMMUTABLE
             AS 'BEGIN RETURN 1; END';";
    assert_err(
        setup,
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (h()) STORED);",
        "generation expression is not immutable",
    );
    assert_err(
        setup,
        "CREATE TABLE t (a int); CREATE INDEX ON t ((a + h()));",
        "functions in index expression must be marked IMMUTABLE",
    );
    build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (f()) STORED,
                 c int GENERATED ALWAYS AS (h('2000-01-01')) STORED);
             CREATE INDEX ON t ((a + f()));",
        ),
    ]);
}
