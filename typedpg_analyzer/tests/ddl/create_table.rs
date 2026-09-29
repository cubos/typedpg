//! CREATE TABLE: column types, constraints (NOT NULL, PRIMARY KEY, UNIQUE,
//! CHECK, FOREIGN KEY, GENERATED), defaults, SERIAL / BIGSERIAL / SMALLSERIAL,
//! type modifiers (VARCHAR(n), NUMERIC(p,s)), IF NOT EXISTS semantics.

use crate::common::*;

// ── Basics ──────────────────────────────────────────────────────────────────

#[test]
fn create_table_basic() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (
            id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT NOT NULL,
            age INT
        );",
    )]);

    let table = snap.resolve_table(Some("public"), "users").unwrap();
    assert_eq!(table.relname, "users");
    assert_eq!(table.relkind, RelKind::Table);
    let attrs = snap.attributes_of(table.oid);
    assert_eq!(attrs.len(), 4);

    let id_col = &attrs[0];
    assert_eq!(id_col.attname, "id");
    assert!(id_col.attnotnull);
    assert!(id_col.atthasdef); // IDENTITY

    let name_col = &attrs[1];
    assert_eq!(name_col.attname, "name");
    assert!(name_col.attnotnull);
    assert!(!name_col.atthasdef);

    let age_col = &attrs[3];
    assert_eq!(age_col.attname, "age");
    assert!(!age_col.attnotnull);
    assert!(!age_col.atthasdef);
}

#[test]
fn create_table_with_default() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            name TEXT NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = &attrs[0];
    assert!(id_col.atthasdef); // SERIAL

    let created_col = &attrs[1];
    assert!(created_col.atthasdef); // DEFAULT now()
    assert!(created_col.attnotnull);

    let name_col = &attrs[2];
    assert!(!name_col.atthasdef);
}

#[test]
fn create_table_registers_composite_and_array_types() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE items (id INT NOT NULL, name TEXT);",
    )]);

    // Composite type for the table.
    let ct = snap.resolve_type_by_name(Some("public"), "items").unwrap();
    assert_eq!(ct.typtype, TypType::Composite);

    // Array type.
    let at = snap.resolve_type_by_name(Some("public"), "_items").unwrap();
    assert_eq!(at.typcategory, TypCategory::Array);
}

#[test]
fn create_table_if_not_exists() {
    let snap = build(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "CREATE TABLE IF NOT EXISTS t (id INT, name TEXT);",
        ),
    ]);

    let table = snap.resolve_table(None, "t").unwrap();
    // Should still have original schema (1 column), not the second one.
    assert_eq!(snap.attributes_of(table.oid).len(), 1);
}

// ── Schema-qualified tables ─────────────────────────────────────────────────

#[test]
fn create_schema_with_table() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA myapp;
         CREATE TABLE myapp.items (id INT NOT NULL, name TEXT NOT NULL);",
    )]);

    let table = snap.resolve_table(Some("myapp"), "items").unwrap();
    assert_eq!(snap.attributes_of(table.oid).len(), 2);
    assert_eq!(snap.namespace_name(table.relnamespace), Some("myapp"));
}

// ── No-op DDL shouldn't fail ────────────────────────────────────────────────

#[test]
fn noops_dont_fail() {
    let _snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL);
         CREATE INDEX idx_t ON t (id);
         CREATE SEQUENCE my_seq;
         GRANT SELECT ON t TO PUBLIC;
         COMMENT ON TABLE t IS 'test table';",
    )]);
}

// ── Duplicates ──────────────────────────────────────────────────────────────

#[test]
fn create_table_duplicate_without_if_not_exists_errors() {
    let result = try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        ("0002.sql", "CREATE TABLE t (name TEXT NOT NULL);"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "relation \"t\" already exists"
    );
}

#[test]
fn create_table_duplicate_column_names_errors() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT, id TEXT);",
    )]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "column \"id\" specified more than once",
    );
}

#[test]
fn create_table_if_not_exists_different_schema_creates_both() {
    // IF NOT EXISTS on a qualified name only skips when an object exists with
    // the SAME schema+name. Different schemas ⇒ two independent tables.
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA other;
         CREATE TABLE public.t (id INT NOT NULL);
         CREATE TABLE IF NOT EXISTS other.t (id INT NOT NULL, name TEXT NOT NULL);",
    )]);

    let t1 = snap.resolve_table(Some("public"), "t").unwrap();
    assert_eq!(snap.attributes_of(t1.oid).len(), 1);

    let t2 = snap.resolve_table(Some("other"), "t").unwrap();
    assert_eq!(snap.attributes_of(t2.oid).len(), 2);
}

// ── SERIAL / BIGSERIAL / SMALLSERIAL ────────────────────────────────────────

#[test]
fn serial_without_pk_is_not_null() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id SERIAL, name TEXT);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = attrs.iter().find(|c| c.attname == "id").unwrap();
    // PG 18 (transformColumnDefinition): serial implies NOT NULL even
    // without a PRIMARY KEY — `attnotnull = t`.
    assert!(id_col.attnotnull, "SERIAL is NOT NULL");
    assert!(id_col.atthasdef, "SERIAL should have a default");
}

#[test]
fn bigserial_resolves_to_int8() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id BIGSERIAL PRIMARY KEY);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = attrs.iter().find(|c| c.attname == "id").unwrap();
    let int8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    assert_eq!(
        id_col.atttypid, int8_oid,
        "BIGSERIAL should resolve to int8"
    );
    assert!(id_col.atthasdef, "BIGSERIAL should have a default");
    assert!(
        id_col.attnotnull,
        "BIGSERIAL PRIMARY KEY should be NOT NULL"
    );
}

#[test]
fn smallserial_resolves_to_int2() {
    let snap = build(&[("0001.sql", "CREATE TABLE t (id SMALLSERIAL NOT NULL);")]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id_col = attrs.iter().find(|c| c.attname == "id").unwrap();
    let int2_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int2")
        .unwrap()
        .oid;
    assert_eq!(
        id_col.atttypid, int2_oid,
        "SMALLSERIAL should resolve to int2"
    );
}

// ── Type modifiers (VARCHAR(n), NUMERIC(p,s)) and common column types ──────

#[test]
fn varchar_and_char_resolve_to_varchar_and_bpchar() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (name VARCHAR(100) NOT NULL, code CHAR(5) NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let name = attrs.iter().find(|c| c.attname == "name").unwrap();
    let code = attrs.iter().find(|c| c.attname == "code").unwrap();

    assert_ne!(name.atttypid.get(), 0, "VARCHAR(100) should resolve");
    assert_ne!(code.atttypid.get(), 0, "CHAR(5) should resolve");

    let varchar_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "varchar")
        .unwrap()
        .oid;
    assert_eq!(name.atttypid, varchar_oid);
}

#[test]
fn numeric_and_decimal_resolve_to_numeric() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (amount NUMERIC(10,2) NOT NULL, factor DECIMAL NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let amount = attrs.iter().find(|c| c.attname == "amount").unwrap();
    let factor = attrs.iter().find(|c| c.attname == "factor").unwrap();

    let numeric_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "numeric")
        .unwrap()
        .oid;
    assert_eq!(amount.atttypid, numeric_oid);
    assert_eq!(
        factor.atttypid, numeric_oid,
        "DECIMAL should resolve to numeric"
    );
}

#[test]
fn datetime_types_resolve() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            a TIMESTAMP NOT NULL,
            b TIMESTAMPTZ NOT NULL,
            c DATE NOT NULL,
            d TIME NOT NULL,
            e INTERVAL NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    for col in attrs {
        assert_ne!(
            col.atttypid.get(),
            0,
            "column '{}' must resolve",
            col.attname
        );
    }
}

#[test]
fn json_and_jsonb_types_resolve() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (data JSONB NOT NULL, meta JSON);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let data = attrs.iter().find(|c| c.attname == "data").unwrap();
    let meta = attrs.iter().find(|c| c.attname == "meta").unwrap();

    let jsonb_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "jsonb")
        .unwrap()
        .oid;
    let json_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "json")
        .unwrap()
        .oid;
    assert_eq!(data.atttypid, jsonb_oid);
    assert_eq!(meta.atttypid, json_oid);
    assert!(!meta.attnotnull, "JSON without NOT NULL should be nullable");
}

#[test]
fn uuid_type_with_default() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id UUID NOT NULL DEFAULT gen_random_uuid(), name TEXT NOT NULL);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let id = attrs.iter().find(|c| c.attname == "id").unwrap();
    let uuid_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "uuid")
        .unwrap()
        .oid;
    assert_eq!(id.atttypid, uuid_oid);
    assert!(
        id.atthasdef,
        "DEFAULT gen_random_uuid() must set has_default"
    );
}

// ── Column-level constraints (parse + NOT NULL propagation) ────────────────

#[test]
fn unique_constraint_does_not_imply_not_null() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id SERIAL PRIMARY KEY,
            email TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    assert_eq!(attrs.len(), 3);
    let email = attrs.iter().find(|c| c.attname == "email").unwrap();
    assert!(email.attnotnull);
}

#[test]
fn foreign_key_constraint_parses_without_affecting_column() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE posts (
             id SERIAL PRIMARY KEY,
             user_id INT NOT NULL REFERENCES users(id),
             title TEXT NOT NULL
         );",
    )]);

    let posts = snap.resolve_table(None, "posts").unwrap();
    let attrs = snap.attributes_of(posts.oid);
    assert_eq!(attrs.len(), 3);
    let user_id = attrs.iter().find(|c| c.attname == "user_id").unwrap();
    assert!(user_id.attnotnull);
}

#[test]
fn check_constraint_parses() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (
            id INT NOT NULL,
            age INT CHECK (age >= 0 AND age <= 200),
            status TEXT NOT NULL CHECK (status IN ('active', 'inactive'))
        );",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(snap.attributes_of(table.oid).len(), 3);
}

#[test]
fn generated_stored_column_has_default() {
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (a INT NOT NULL, b INT GENERATED ALWAYS AS (a * 2) STORED);",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    let attrs = snap.attributes_of(table.oid);
    let b = attrs.iter().find(|c| c.attname == "b").unwrap();
    assert!(
        b.atthasdef,
        "GENERATED ALWAYS AS (stored) must set has_default"
    );
}

// ── VOLATILE function rejection (CHECK / GENERATED / index expressions) ────
//
// PG forbids VOLATILE functions (random(), gen_random_uuid(), nextval(), …)
// from CHECK constraints, `GENERATED … STORED` expressions, and index
// expressions — otherwise the constraint/index/generated value could
// disagree with itself between rows or scans.
//
// We don't model `pg_proc.provolatile` (every pg_catalog function would
// need an extra column) so the volatility check is name-based against a
// hard-coded allow-list of well-known VOLATILE functions.

#[test]
fn volatile_function_in_check_constraint_is_accepted_at_ddl_time() {
    // Despite documentation suggesting CHECK constraints should be
    // IMMUTABLE, PG does not enforce this at DDL time — the constraint is
    // accepted and only flagged at runtime if PG actually trips on the
    // mutability. The analyzer mirrors this so it doesn't reject DDL that
    // PG happily accepts.
    try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (id INT NOT NULL CHECK (id < (random() * 100)::int));",
    )])
    .expect("PG accepts volatile-in-CHECK at DDL time, so the analyzer must too");
}

#[test]
fn volatile_function_in_generated_stored_column_should_error() {
    // PG: `generation expression is not immutable`. The expression must be
    // pure of the row's own columns.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (
                id INT NOT NULL,
                noise NUMERIC GENERATED ALWAYS AS (random() * id) STORED
            );",
        )]),
        DdlError::UnsupportedDdl(_),
        "generation expression is not immutable: function \"random\" must be marked IMMUTABLE",
    );
}

#[test]
fn volatile_function_in_table_level_check_constraint_is_accepted() {
    // Table-level CHECK behaves identically to column-level: PG accepts
    // volatile expressions at DDL time; the analyzer mirrors that.
    try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (
            id INT NOT NULL,
            CHECK (id < (random() * 100)::int)
        );",
    )])
    .expect("PG accepts volatile-in-CHECK at DDL time");
}

#[test]
fn alter_table_add_volatile_check_constraint_is_accepted() {
    try_apply(&[
        ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
        (
            "0002.sql",
            "ALTER TABLE t ADD CONSTRAINT chk CHECK (id > random()::int);",
        ),
    ])
    .expect("PG accepts volatile-in-CHECK at DDL time");
}

#[test]
fn check_constraint_calling_immutable_function_is_accepted() {
    // The volatility check must not reject everyday IMMUTABLE functions —
    // length, abs, lower, etc. show up routinely in CHECK constraints.
    try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (
            id INT NOT NULL,
            name TEXT NOT NULL CHECK (length(name) > 0)
         );",
    )])
    .expect("length() is IMMUTABLE — must be accepted");
}

#[test]
fn nested_volatile_call_in_check_is_accepted() {
    // PG doesn't run the volatility walk on CHECK at DDL time, so even a
    // VOLATILE call buried inside COALESCE / NULLIF / arithmetic / casts
    // sails through. The analyzer matches that behavior.
    try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (
            id INT NOT NULL CHECK (id > COALESCE(NULLIF((random() * 10)::int, 0), 1))
        );",
    )])
    .expect("PG accepts volatile-in-CHECK at DDL time");
}

#[test]
fn nextval_in_check_constraint_is_accepted_at_ddl_time() {
    // `nextval` is VOLATILE, but PG still accepts it inside a CHECK at DDL
    // time — only runtime evaluation may flag it.
    try_apply(&[
        ("0001.sql", "CREATE SEQUENCE seq;"),
        (
            "0002.sql",
            "CREATE TABLE t (id INT NOT NULL CHECK (id < nextval('seq')::int));",
        ),
    ])
    .expect("PG accepts volatile-in-CHECK at DDL time");
}

#[test]
fn gen_random_uuid_in_generated_column_is_rejected() {
    // `gen_random_uuid` is VOLATILE — generated columns must be pure.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (
                id INT NOT NULL,
                token UUID GENERATED ALWAYS AS (gen_random_uuid()) STORED
            );",
        )]),
        DdlError::UnsupportedDdl(_),
        "generation expression is not immutable: function \"gen_random_uuid\" must be marked IMMUTABLE",
    );
}

// ── CHECK constraint must produce boolean (PG: argument of CHECK must be
// type boolean, not type X). We type-check the parsed expression against
// the freshly-built table's columns and reject anything that isn't bool.

#[test]
fn check_constraint_returning_int_is_rejected() {
    // `CHECK (id)` — `id` is int, not boolean.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE t (id INT NOT NULL CHECK (id));",)]),
        DdlError::UnsupportedDdl(_),
        "argument of CHECK must be type boolean, not type integer (CHECK constraint on t.id)",
    );
}

#[test]
fn check_constraint_returning_text_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL CHECK (name));",
        )]),
        DdlError::UnsupportedDdl(_),
        "argument of CHECK must be type boolean, not type text (CHECK constraint on t.name)",
    );
}

#[test]
fn table_level_check_returning_int_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (
                id  INT NOT NULL,
                qty INT NOT NULL,
                CHECK (id + qty)
            );",
        )]),
        DdlError::UnsupportedDdl(_),
        "argument of CHECK must be type boolean, not type integer (table-level CHECK constraint on \"t\")",
    );
}

#[test]
fn check_constraint_returning_bool_expression_is_accepted() {
    // Sanity: the type check must not reject legitimate boolean CHECKs.
    try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (
            id    INT  NOT NULL CHECK (id > 0),
            label TEXT NOT NULL CHECK (length(label) > 0),
            qty   INT  NOT NULL,
            CHECK (id < qty)
         );",
    )])
    .expect("boolean CHECK expressions must be accepted");
}

#[test]
fn alter_table_add_check_returning_int_is_rejected() {
    assert_ddl_err!(
        try_apply(&[
            ("0001.sql", "CREATE TABLE t (id INT NOT NULL);"),
            ("0002.sql", "ALTER TABLE t ADD CONSTRAINT chk CHECK (id);"),
        ]),
        DdlError::UnsupportedDdl(_),
        "argument of CHECK must be type boolean, not type integer (CHECK constraint on \"t\")",
    );
}

#[test]
fn check_constraint_referencing_unknown_column_is_rejected() {
    // PG: `column "ghost" does not exist`. The CHECK type-checker walks
    // the expression in the table's scope, so unknown column references
    // are caught here too.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (id INT NOT NULL CHECK (ghost > 0));",
        )]),
        DdlError::UnsupportedDdl(_),
        "column \"ghost\" does not exist (in CHECK constraint on t.id)",
    );
}

// ── GENERATED expression must be assignable to the column's declared type ──

#[test]
fn generated_column_with_mismatched_type_is_rejected() {
    // The expression returns text (`upper`), but the column is declared
    // INT — assignment goal fails the check.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (
                id    INT  NOT NULL,
                label TEXT NOT NULL,
                bad   INT  GENERATED ALWAYS AS (upper(label)) STORED
            );",
        )]),
        DdlError::UnsupportedDdl(_),
        "column \"bad\" is of type integer but default expression is of type text (You will need \
         to rewrite or cast the expression.)",
    );
}

#[test]
fn generated_column_with_compatible_type_is_accepted() {
    // `upper(label)` returns text — fits a text column. Sanity for the
    // type check: it must not over-reject.
    try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (
            id    INT  NOT NULL,
            label TEXT NOT NULL,
            upper_label TEXT GENERATED ALWAYS AS (upper(label)) STORED
         );",
    )])
    .expect("type-matching GENERATED must be accepted");
}

#[test]
fn generated_column_with_assignable_numeric_widening_is_accepted() {
    // PG widens int → bigint via assignment cast — the analyzer mirrors that.
    try_apply(&[(
        "0001.sql",
        "CREATE TABLE t (
            id INT NOT NULL,
            big_id BIGINT GENERATED ALWAYS AS (id) STORED
         );",
    )])
    .expect("int → bigint assignment must be accepted in a generated column");
}

#[test]
fn generated_column_referencing_unknown_column_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t (
                id INT NOT NULL,
                bad INT GENERATED ALWAYS AS (ghost + 1) STORED
            );",
        )]),
        DdlError::UnsupportedDdl(_),
        "column \"ghost\" does not exist (in GENERATED expression on t.bad)",
    );
}

// ── Param inference after DDL (GROUP BY / HAVING context) ───────────────────

#[test]
fn param_in_group_by_and_having_is_inferred() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE orders (id BIGINT, total INT NOT NULL);",
    )]);

    let info = db
        .analyze(
            "SELECT total, COUNT(*) AS c
             FROM orders
             GROUP BY total
             HAVING COUNT(*) > $min",
        )
        .expect("HAVING param should be resolvable");
    assert_eq!(info.params.len(), 1);
    assert_eq!(
        info.params[0].pg_type,
        Type::Basic {
            schema: "pg_catalog".into(),
            name: "int8".into(),
            extension: None,
            typmod: None,
            collation: None,
        }
    );
}

// ── Type name resolution (PG `typenameTypeId`) ──────────────────────────────

#[test]
fn unknown_column_type_is_rejected() {
    // PG 18: ERROR 42704 type "nosuchtype" does not exist.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE t (a nosuchtype);")]),
        DdlError::TypeNotFound(_),
        "type \"nosuchtype\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE t (a nosuch[]);")]),
        DdlError::TypeNotFound(_),
        "type \"nosuch[]\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE t (a public.nosuch);")]),
        DdlError::TypeNotFound(_),
        "type \"public.nosuch\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE t (a nosch.typ);")]),
        DdlError::TypeNotFound(_),
        "schema \"nosch\" does not exist",
    );
    // A quoted keyword spelling is a plain identifier, not the int4 alias.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE t (a \"integer\");")]),
        DdlError::TypeNotFound(_),
        "type \"integer\" does not exist",
    );
}

#[test]
fn type_outside_search_path_is_not_visible() {
    // PG 18: the enum lives in `s`, which is not on the search path.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE SCHEMA s; CREATE TYPE s.mood AS ENUM ('a'); CREATE TABLE t2 (m mood);",
        )]),
        DdlError::TypeNotFound(_),
        "type \"mood\" does not exist",
    );
}

#[test]
fn alter_table_with_unknown_type_is_rejected() {
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t3 (a int); ALTER TABLE t3 ADD COLUMN b nosuchtype;",
        )]),
        DdlError::TypeNotFound(_),
        "type \"nosuchtype\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE t3 (a int); ALTER TABLE t3 ALTER COLUMN a TYPE nosuchtype;",
        )]),
        DdlError::TypeNotFound(_),
        "type \"nosuchtype\" does not exist",
    );
}

#[test]
fn quoted_char_is_the_internal_single_byte_type() {
    // PG 18 `\d t`: a "char", b character(1), c bpchar.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a \"char\", b char, c \"bpchar\");",
    )]);
    let info = db.analyze("SELECT a, b, c FROM t").unwrap();
    assert_cols(
        &info,
        vec![
            cn("a", basic("pg_catalog", "char")),
            cn("b", basic_with_typmod("pg_catalog", "bpchar", 5)),
            cn("c", bpchar()),
        ],
    );
}

// ── CREATE TABLE ... OF type / LIKE ─────────────────────────────────────────

#[test]
fn typed_table_gets_the_type_columns() {
    // PG 18 `\d people`: name text not null, age integer.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TYPE person AS (name text, age int);
         CREATE TABLE people OF person (name NOT NULL);
         CREATE TABLE people4 OF person (name WITH OPTIONS DEFAULT 'x', age PRIMARY KEY);",
    )]);
    assert_cols(
        &db.analyze("SELECT * FROM people").unwrap(),
        vec![c("name", text()), cn("age", int4())],
    );
    assert_cols(
        &db.analyze("SELECT * FROM people4").unwrap(),
        vec![cn("name", text()), c("age", int4())],
    );
}

#[test]
fn typed_table_errors() {
    // PG 18: 42703 column "extra" does not exist; 42809 type integer is not
    // a composite type.
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TYPE person AS (name text, age int);
             CREATE TABLE people2 OF person (extra NOT NULL);",
        )]),
        DdlError::Parse(_),
        "column \"extra\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE people3 OF int4;")]),
        DdlError::Parse(_),
        "type integer is not a composite type",
    );
}

#[test]
fn like_copies_the_source_columns() {
    // PG 18: u has a integer not null, b integer — with or without INCLUDING.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int NOT NULL, b int);
         CREATE TABLE u (LIKE t INCLUDING ALL);
         CREATE TABLE u2 (LIKE t);
         CREATE TABLE u3 (x int, LIKE t, y text);",
    )]);
    let expected = vec![c("a", int4()), cn("b", int4())];
    assert_cols(&db.analyze("SELECT * FROM u").unwrap(), expected.clone());
    assert_cols(&db.analyze("SELECT a, b FROM u2").unwrap(), expected);
    assert_cols(
        &db.analyze("SELECT * FROM u3").unwrap(),
        vec![
            cn("x", int4()),
            c("a", int4()),
            cn("b", int4()),
            cn("y", text()),
        ],
    );
}

#[test]
fn like_copies_defaults_generated_and_identity_only_when_included() {
    // PG 18: plain LIKE drops the default, generation expression and
    // identity (a NOT NULL stays), so `b` becomes an ordinary column and
    // `i` must be supplied. INCLUDING ALL keeps them.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE p (a int NOT NULL DEFAULT 1,
                         b text GENERATED ALWAYS AS ('x') STORED,
                         i int GENERATED ALWAYS AS IDENTITY);
         CREATE TABLE lk (LIKE p);
         CREATE TABLE lk2 (LIKE p INCLUDING ALL);",
    )]);
    let lk = class_oid(&db, Some("public"), "lk");
    let attrs = db.attributes_of(lk);
    assert!(attrs.iter().all(|a| !a.atthasdef), "{attrs:?}");
    assert!(attrs.iter().all(|a| a.attgenerated.is_none()));
    assert!(attrs.iter().all(|a| a.attidentity.is_none()));
    assert!(attrs.iter().find(|a| a.attname == "i").unwrap().attnotnull);
    let lk2 = class_oid(&db, Some("public"), "lk2");
    let attrs = db.attributes_of(lk2);
    assert!(attrs.iter().find(|a| a.attname == "a").unwrap().atthasdef);
    assert!(
        attrs
            .iter()
            .find(|a| a.attname == "b")
            .unwrap()
            .attgenerated
            .is_some()
    );
    assert!(
        attrs
            .iter()
            .find(|a| a.attname == "i")
            .unwrap()
            .attidentity
            .is_some()
    );
    // The generated column is writable in the plain copy.
    db.analyze("INSERT INTO lk (a, b, i) VALUES (1, 'y', 2)")
        .unwrap();
}

#[test]
fn like_including_indexes_copies_the_keys() {
    // PG 18 `\d w`: "w_pkey" PRIMARY KEY (a), "w_b_key" UNIQUE CONSTRAINT (b).
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE u (a int PRIMARY KEY, b int UNIQUE, c int CHECK (c > 0));
         CREATE TABLE w (LIKE u INCLUDING ALL);",
    )]);
    db.analyze(
        "INSERT INTO w (a, b, c) VALUES (1, 2, 3) ON CONFLICT ON CONSTRAINT w_pkey DO NOTHING",
    )
    .unwrap();
    db.analyze("INSERT INTO w (a, b, c) VALUES (1, 2, 3) ON CONFLICT (b) DO NOTHING")
        .unwrap();
}

#[test]
fn like_errors() {
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE lk7 (LIKE nosuch);")]),
        DdlError::TableNotFound(_),
        "relation \"nosuch\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE SEQUENCE sq; CREATE TABLE lk6 (LIKE sq);",
        )]),
        DdlError::Parse(_),
        "relation \"sq\" is invalid in LIKE clause",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE p (a int); CREATE TABLE lk3 (a int, LIKE p);",
        )]),
        DdlError::DuplicateObject(_),
        "column \"a\" specified more than once",
    );
}

#[test]
fn primary_key_on_missing_column_is_rejected() {
    // PG 18: 42703 column "b" named in key does not exist.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE TABLE t (a int, PRIMARY KEY (b));")]),
        DdlError::Parse(_),
        "column \"b\" named in key does not exist",
    );
}

// ── DEFAULT expressions (cookDefault) ───────────────────────────────────────

#[test]
fn default_expressions_are_type_checked() {
    // PG 18 wording for each rejected DEFAULT.
    for (sql, msg) in [
        (
            "CREATE TABLE t (a int DEFAULT now());",
            "column \"a\" is of type integer but default expression is of type timestamp with time zone",
        ),
        (
            "CREATE TABLE t (a int DEFAULT 'abc');",
            "invalid input syntax for type integer: \"abc\"",
        ),
        (
            "CREATE TABLE t (a int, b int DEFAULT a);",
            "cannot use column reference in DEFAULT expression",
        ),
        (
            "CREATE TABLE t (a int DEFAULT (SELECT 1));",
            "cannot use subquery in DEFAULT expression",
        ),
        (
            "CREATE TABLE t2 (a int DEFAULT count(*));",
            "aggregate functions are not allowed in DEFAULT expressions",
        ),
        (
            "CREATE TABLE t3 (a int DEFAULT nosuchfn());",
            "function nosuchfn() does not exist",
        ),
        (
            "CREATE TABLE a (x int); ALTER TABLE a ALTER COLUMN x SET DEFAULT 'abc';",
            "invalid input syntax for type integer: \"abc\"",
        ),
        (
            "CREATE TABLE a (x int); ALTER TABLE a ALTER COLUMN x SET DEFAULT now();",
            "column \"x\" is of type integer but default expression is of type timestamp with time zone",
        ),
        (
            "CREATE TABLE a (x int); ALTER TABLE a ADD COLUMN y int DEFAULT now();",
            "column \"y\" is of type integer but default expression is of type timestamp with time zone",
        ),
        (
            "CREATE DOMAIN d AS int DEFAULT 'abc';",
            "invalid input syntax for type integer: \"abc\"",
        ),
        (
            "CREATE DOMAIN d2 AS int DEFAULT now();",
            "column \"d2\" is of type integer but default expression is of type timestamp with time zone",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
}

#[test]
fn assignment_compatible_defaults_are_accepted() {
    // PG 18 accepts all of these (assignment casts, untyped literals, NULL).
    build_db(&[(
        "0001.sql",
        "CREATE TABLE t (a int DEFAULT 1.5, b text DEFAULT 5, c int DEFAULT '7',
                         d int[] DEFAULT '{}', e date DEFAULT now(), f bigint DEFAULT 1,
                         g int DEFAULT NULL, h bool DEFAULT 'yes', i numeric DEFAULT 2::bigint,
                         j timestamptz DEFAULT now(), k uuid DEFAULT gen_random_uuid());",
    )]);
}

// ── Column collation defaults to the type's (GetColumnDefCollation) ─────────

#[test]
fn domain_collation_reaches_its_columns() {
    // PG 18: t.a's attcollation is the domain's "C"; an explicit COLLATE
    // wins.
    let db = build_db(&[(
        "0001.sql",
        "CREATE DOMAIN d AS varchar(5) COLLATE \"C\";
         CREATE TABLE t (a d, b d COLLATE \"POSIX\");",
    )]);
    let info = db.analyze("SELECT a, b FROM t").unwrap();
    let colls: Vec<Option<String>> = info
        .columns
        .iter()
        .map(|c| match &c.pg_type {
            Type::Domain { collation, .. } => collation.clone(),
            other => panic!("expected a domain, got {other:?}"),
        })
        .collect();
    assert_eq!(colls, vec![Some("C".to_owned()), Some("POSIX".to_owned())]);
}

#[test]
fn view_columns_keep_their_expression_collation() {
    // PG 18: the view column has attcollation "C", like the table's.
    let db = build_db(&[(
        "0001.sql",
        "CREATE TABLE b (t text COLLATE \"C\");
         CREATE VIEW vb AS SELECT t FROM b;
         CREATE TYPE comp AS (n text COLLATE \"C\");",
    )]);
    let info = db.analyze("SELECT t FROM vb").unwrap();
    assert_cols(
        &info,
        vec![cn("t", basic_with_collation("pg_catalog", "text", "C"))],
    );
}

#[test]
fn inherited_column_collation_must_match() {
    // PG 18: 42P21 column "x" has a collation conflict ("C" versus "default").
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE TABLE s2 (x text COLLATE \"C\"); CREATE TABLE c7 (x text) INHERITS (s2);",
        )]),
        DdlError::Parse(_),
        "column \"x\" has a collation conflict",
    );
}

// ── CREATE TABLE column / key validations (transformColumnDefinition & co.) ─

#[test]
fn create_table_column_definition_conflicts_are_rejected() {
    for (sql, msg) in [
        (
            "CREATE TABLE t1 (a int PRIMARY KEY, b int PRIMARY KEY);",
            "multiple primary keys for table \"t1\" are not allowed",
        ),
        (
            "CREATE TABLE t1 (a int PRIMARY KEY, b int, PRIMARY KEY (b));",
            "multiple primary keys for table \"t1\" are not allowed",
        ),
        (
            "CREATE TABLE t2 (id text GENERATED ALWAYS AS IDENTITY);",
            "identity column type must be smallint, integer, or bigint",
        ),
        (
            "CREATE TABLE t3 (a int, b int GENERATED ALWAYS AS (a) STORED, c int GENERATED ALWAYS AS (b) STORED);",
            "cannot use generated column \"b\" in column generation expression",
        ),
        (
            "CREATE TABLE t4 (a int, b int DEFAULT 1 GENERATED ALWAYS AS (a) STORED);",
            "both default and generation expression specified for column \"b\" of table \"t4\"",
        ),
        (
            "CREATE TABLE t5 (a int DEFAULT 1 GENERATED ALWAYS AS IDENTITY);",
            "both default and identity specified for column \"a\" of table \"t5\"",
        ),
        (
            "CREATE TABLE m3 (id int) PARTITION BY RANGE (nosuch);",
            "column \"nosuch\" named in partition key does not exist",
        ),
        (
            "CREATE TABLE m (id int PRIMARY KEY, d date) PARTITION BY RANGE (d);",
            "unique constraint on partitioned table must include all partitioning columns",
        ),
        (
            "CREATE TABLE m2 (id int, d date, UNIQUE (id)) PARTITION BY RANGE (d);",
            "unique constraint on partitioned table must include all partitioning columns",
        ),
        (
            "CREATE TABLE m4 (id int, d date) PARTITION BY RANGE (d);
             CREATE UNIQUE INDEX ON m4 (id);",
            "unique constraint on partitioned table must include all partitioning columns",
        ),
        (
            "CREATE TABLE m5 (id int, d date) PARTITION BY RANGE (d);
             ALTER TABLE m5 ADD PRIMARY KEY (id);",
            "unique constraint on partitioned table must include all partitioning columns",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    // A key covering the partition columns is fine.
    build_db(&[(
        "0001.sql",
        "CREATE TABLE m (id int, d date, PRIMARY KEY (id, d)) PARTITION BY RANGE (d);
         CREATE TABLE s (id smallint GENERATED BY DEFAULT AS IDENTITY, b bigint GENERATED ALWAYS AS IDENTITY);",
    )]);
}
