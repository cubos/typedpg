//! CREATE / ALTER / DROP EXTENSION: version selection, schema placement,
//! objects (types, functions, casts) materialized from the extension bundle.

use crate::common::*;

// ── CREATE EXTENSION — bundled extensions ──────────────────────────────────

#[test]
fn create_extension_uuid_ossp() {
    let snap = build(&[("0001.sql", "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\";")]);

    let fns = snap.find_functions(None, "uuid_generate_v4");
    assert!(!fns.is_empty());
    let uuid_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "uuid")
        .unwrap()
        .oid;
    assert_eq!(fns[0].prorettype, uuid_oid);
}

#[test]
fn create_extension_citext() {
    let snap = build(&[("0001.sql", "CREATE EXTENSION citext;")]);

    let te = snap.resolve_type_by_name(None, "citext").unwrap();
    assert_eq!(te.typtype, TypType::Base);
    assert!(
        snap.resolve_type_by_name(Some("public"), "_citext")
            .is_some()
    );

    // Should have implicit cast citext -> text.
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    assert!(snap.has_implicit_cast(te.oid, text_oid));
}

#[test]
fn create_extension_vector_registers_operators() {
    // pgvector's CREATE OPERATOR statements (e.g. `<=>`, `<->`) must be
    // registered in the snapshot so expressions using them type-check.
    let snap = build(&[("0001.sql", "CREATE EXTENSION vector;")]);

    let vector_oid = snap
        .resolve_type_by_name(None, "vector")
        .expect("vector type should be registered")
        .oid;
    let float8_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "float8")
        .unwrap()
        .oid;

    // Cosine distance `<=>` should resolve with (vector, vector) operands.
    let op = snap
        .find_operator("<=>", Some(vector_oid), vector_oid)
        .expect("'<=>' operator should be registered for (vector, vector)");
    assert_eq!(op.left_type_oid, Some(vector_oid));
    assert_eq!(op.right_type_oid, vector_oid);
    assert_eq!(
        op.result_type_oid, float8_oid,
        "cosine_distance returns float8"
    );

    // L2 distance `<->` too.
    assert!(
        snap.find_operator("<->", Some(vector_oid), vector_oid)
            .is_some()
    );
}

#[test]
fn analyze_vector_query_maps_to_pgvector_rust_type() {
    // End-to-end: a SELECT on a vector column and a `<=>` expression should
    // produce Rust types routed to the `pgvector` crate. `typedpg` itself
    // does not depend on pgvector — the type path is emitted as a string.
    let db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         CREATE TABLE items (
             id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             embedding vector NOT NULL
         );",
    )]);

    // Column of type `vector` surfaces as a Basic type tagged with the
    // extension name; the macro crate is responsible for routing it to
    // `pgvector::Vector`.
    let info = db.analyze("SELECT id, embedding FROM items").unwrap();
    let embedding = info.columns.iter().find(|c| c.name == "embedding").unwrap();
    assert_eq!(
        embedding.pg_type,
        Type::Basic {
            schema: "public".into(),
            name: "vector".into(),
            extension: Some("vector".into()),
            typmod: None,
            collation: None,
        }
    );
    assert!(!embedding.nullable);

    // A query using the `<=>` operator infers the parameter's type as vector
    // via the operator's left operand; `Type::cast_name` reports the PG name
    // so the macro can emit `::vector` in the rewritten SQL.
    let info = db
        .analyze("SELECT id, (embedding <=> $q) AS dist FROM items ORDER BY embedding <=> $q")
        .unwrap();
    let dist = info.columns.iter().find(|c| c.name == "dist").unwrap();
    assert_eq!(
        dist.pg_type,
        Type::Basic {
            schema: "pg_catalog".into(),
            name: "float8".into(),
            extension: None,
            typmod: None,
            collation: None,
        },
        "cosine_distance returns float8"
    );
    assert_eq!(info.params.len(), 1);
    assert_eq!(
        info.params[0].pg_type,
        Type::Basic {
            schema: "public".into(),
            name: "vector".into(),
            extension: Some("vector".into()),
            typmod: None,
            collation: None,
        }
    );
    assert_eq!(
        info.params[0].pg_type.cast_name().as_deref(),
        Some("public.vector"),
    );
}

#[test]
fn create_extension_tags_types_with_extension_name() {
    // Types created by `CREATE EXTENSION` must carry the extension name so
    // the Rust type mapper can route them to crate-specific Rust types.
    let snap = build(&[("0001.sql", "CREATE EXTENSION vector;")]);

    let vector = snap
        .resolve_type_by_name(None, "vector")
        .expect("vector type should be registered");
    assert_eq!(snap.extension_of_type(vector.oid), Some("vector"));

    let halfvec = snap
        .resolve_type_by_name(None, "halfvec")
        .expect("halfvec type should be registered");
    assert_eq!(snap.extension_of_type(halfvec.oid), Some("vector"));

    let sparsevec = snap
        .resolve_type_by_name(None, "sparsevec")
        .expect("sparsevec type should be registered");
    assert_eq!(snap.extension_of_type(sparsevec.oid), Some("vector"));

    // User-defined types (from the same migration) must NOT be tagged.
    let snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         CREATE TYPE my_thing AS ENUM ('a', 'b');",
    )]);
    let my_thing = snap
        .resolve_type_by_name(None, "my_thing")
        .expect("my_thing should exist");
    assert_eq!(snap.extension_of_type(my_thing.oid), None);
}

#[test]
fn create_extension_with_schema() {
    let snap = build(&[(
        "0001.sql",
        "CREATE SCHEMA extensions;
         CREATE EXTENSION \"uuid-ossp\" SCHEMA extensions;",
    )]);

    // Functions should be in the 'extensions' schema.
    let fns = snap.find_functions(Some("extensions"), "uuid_generate_v4");
    assert!(
        !fns.is_empty(),
        "uuid_generate_v4 should be in 'extensions' schema"
    );
}

// ── Extension versioning ────────────────────────────────────────────────────

#[test]
fn extension_version_creates_at_default() {
    let snap = build(&[("0001.sql", "CREATE EXTENSION citext;")]);

    // citext should be installed — check the type exists.
    let te = snap.resolve_type_by_name(None, "citext").unwrap();
    assert_eq!(te.typtype, TypType::Base);

    // Functions from upgrade scripts should also be present.
    let fns = snap.find_functions(None, "citext_hash_extended");
    assert!(
        !fns.is_empty(),
        "citext_hash_extended should exist (from 1.5->1.6 upgrade)"
    );
}

#[test]
fn extension_version_specific() {
    // Install citext at version 1.4 (base only, no upgrades).
    let snap = build(&[("0001.sql", "CREATE EXTENSION citext VERSION '1.4';")]);

    let te = snap.resolve_type_by_name(None, "citext").unwrap();
    assert_eq!(te.typtype, TypType::Base);

    // citext_hash_extended is from 1.5->1.6 upgrade, should NOT exist.
    let fns = snap.find_functions(None, "citext_hash_extended");
    assert!(
        fns.is_empty(),
        "citext_hash_extended should NOT exist at version 1.4"
    );
}

#[test]
fn extension_alter_update() {
    // Install at 1.4, then upgrade to 1.6.
    let snap = build(&[
        ("0001.sql", "CREATE EXTENSION citext VERSION '1.4';"),
        ("0002.sql", "ALTER EXTENSION citext UPDATE TO '1.6';"),
    ]);

    // citext_hash_extended is from 1.5->1.6, should now exist.
    let fns = snap.find_functions(None, "citext_hash_extended");
    assert!(
        !fns.is_empty(),
        "citext_hash_extended should exist after upgrade to 1.6"
    );

    // citext_pattern_lt is from 1.4->1.5, should also exist.
    let fns = snap.find_functions(None, "citext_pattern_lt");
    assert!(
        !fns.is_empty(),
        "citext_pattern_lt should exist after upgrade through 1.5"
    );
}

#[test]
fn extension_if_not_exists() {
    let snap = build(&[
        ("0001.sql", "CREATE EXTENSION citext;"),
        ("0002.sql", "CREATE EXTENSION IF NOT EXISTS citext;"),
    ]);

    // Should not error, just skip.
    let te = snap.resolve_type_by_name(None, "citext").unwrap();
    assert_eq!(te.typtype, TypType::Base);
}

#[test]
fn create_extension_duplicate_without_if_not_exists_errors() {
    // PG: re-installing an extension without IF NOT EXISTS errors — silently
    // reinstalling would duplicate all its types and functions.
    let result = try_apply(&[
        ("0001.sql", "CREATE EXTENSION \"uuid-ossp\";"),
        ("0002.sql", "CREATE EXTENSION \"uuid-ossp\";"),
    ]);

    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "extension \"uuid-ossp\" already exists"
    );
}

#[test]
fn extension_unknown_is_error() {
    // Unknown extensions are rejected: the analyzer has no way to know what
    // types or functions they register, so silently accepting them would
    // hide type errors downstream.
    let result = try_apply(&[(
        "0001.sql",
        "CREATE EXTENSION IF NOT EXISTS some_unknown_ext;",
    )]);
    assert_ddl_err!(
        result,
        DdlError::ExtensionError(_),
        "extension \"some_unknown_ext\" is not available (unknown to the analyzer: add its SQL script to typedpg_analyzer/src/extensions/ to register it for static analysis)"
    );
}

// ── Param inference over extension-provided types and operators ─────────────

#[test]
fn param_only_in_order_by_is_inferred() {
    // A parameter that appears exclusively in the ORDER BY clause must
    // still get its type inferred from the expression context. Regression:
    // previously ORDER BY was skipped entirely, so `$embedding` below was
    // reported as UNKNOWN.
    let db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         CREATE TABLE items (
             id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             embedding vector NOT NULL
         );",
    )]);

    let info = db
        .analyze("SELECT id FROM items ORDER BY embedding <=> $q LIMIT 10")
        .expect("ORDER BY param should be resolvable");
    assert_eq!(info.params.len(), 1);
    assert_eq!(
        info.params[0].pg_type,
        Type::Basic {
            schema: "public".into(),
            name: "vector".into(),
            extension: Some("vector".into()),
            typmod: None,
            collation: None,
        }
    );
}

#[test]
fn create_extension_hstore_ltree_cube_isn() {
    // PG 18 installs all of these; their scripts use ALTER TYPE ... SET
    // (subscript / receive / ...) and many same-named I/O functions.
    let db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION hstore;
         CREATE EXTENSION ltree;
         CREATE EXTENSION cube;
         CREATE EXTENSION earthdistance;
         CREATE EXTENSION isn;
         CREATE TABLE t (h hstore, l ltree, c cube, e ean13);",
    )]);
    let info = db.analyze("SELECT h, l, c, e FROM t").unwrap();
    let names: Vec<String> = info
        .columns
        .iter()
        .map(|c| c.pg_type.cast_name().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        vec![
            "public.hstore",
            "public.ltree",
            "public.cube",
            "public.ean13"
        ]
    );
}

#[test]
fn extension_members_only_go_with_their_extension() {
    for (sql, message) in [
        (
            "CREATE EXTENSION pgcrypto; DROP FUNCTION gen_salt(text);",
            "cannot drop function gen_salt(text) because extension pgcrypto requires it",
        ),
        (
            "CREATE EXTENSION hstore; DROP TYPE hstore CASCADE;",
            "cannot drop type hstore because extension hstore requires it",
        ),
        (
            "CREATE EXTENSION hstore; CREATE TABLE th (a hstore, b int); DROP EXTENSION hstore;",
            "cannot drop extension hstore because other objects depend on it",
        ),
        (
            "CREATE EXTENSION pg_trgm;
             CREATE TABLE tt (d text);
             CREATE INDEX ON tt USING gin (d gin_trgm_ops);
             DROP EXTENSION pg_trgm;",
            "cannot drop extension pg_trgm because other objects depend on it",
        ),
        (
            "DROP EXTENSION plpgsql;
             CREATE FUNCTION pf() RETURNS int LANGUAGE plpgsql AS 'begin return 1; end';",
            "language \"plpgsql\" does not exist",
        ),
        (
            "CREATE EXTENSION pg_trgm;
             CREATE TABLE tt (d text);
             CREATE INDEX ON tt USING gist (d gist_trgm_ops(siglen = 99999));",
            "value 99999 out of bounds for option \"siglen\"",
        ),
        (
            "CREATE TABLE tt (d text); CREATE INDEX ON tt (d text_ops(x = 1));",
            "operator class text_ops has no options",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(message), "{sql}\n  got: {err}");
    }
}

#[test]
fn drop_extension_or_schema_cascade_drops_columns_of_their_types() {
    let db = build(&[(
        "0001.sql",
        "CREATE EXTENSION hstore;
         CREATE TABLE th (a hstore, b int);
         DROP EXTENSION hstore CASCADE;
         CREATE SCHEMA s;
         CREATE TYPE s.mood AS ENUM ('a');
         CREATE DOMAIN s.dd AS int;
         CREATE TABLE t (m s.mood, d s.dd, b int);
         DROP SCHEMA s CASCADE;
         DROP EXTENSION plpgsql;
         CREATE EXTENSION plpgsql;
         CREATE FUNCTION pf() RETURNS int LANGUAGE plpgsql AS 'begin return 1; end';
         CREATE EXTENSION pg_trgm;
         CREATE TABLE tt (d text);
         CREATE INDEX ON tt USING gist (d gist_trgm_ops(siglen = 12));",
    )]);
    for table in ["th", "t"] {
        let q = db.analyze(&format!("SELECT * FROM {table}")).unwrap();
        let names: Vec<&str> = q.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["b"], "{table}");
    }
}

#[test]
fn routines_depend_on_their_language() {
    // PG 18: a plpgsql routine depends on language plpgsql, a member of the
    // plpgsql extension; the pinned built-in languages record nothing.
    let setup = "CREATE FUNCTION pf() RETURNS int LANGUAGE plpgsql AS 'begin return 1; end';
                 CREATE PROCEDURE pp() LANGUAGE plpgsql AS 'begin end';
                 CREATE FUNCTION sf() RETURNS int LANGUAGE sql AS 'select 1';";
    for (stmt, message) in [
        (
            "DROP EXTENSION plpgsql;",
            "cannot drop extension plpgsql because other objects depend on it (function pf() \
             depends on language plpgsql)",
        ),
        (
            "DROP LANGUAGE plpgsql;",
            "cannot drop language plpgsql because extension plpgsql requires it (You can drop \
             extension plpgsql instead.)",
        ),
        (
            "DROP LANGUAGE sql;",
            "cannot drop language sql because it is required by the database system",
        ),
        (
            "DROP FUNCTION plpgsql_call_handler();",
            "cannot drop function plpgsql_call_handler() because extension plpgsql requires it",
        ),
        (
            "DROP EXTENSION plpgsql CASCADE; DO 'begin end';",
            "language \"plpgsql\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(message), "{stmt}\n  got: {err}");
    }
    // CASCADE takes the plpgsql routines only; CREATE EXTENSION brings the
    // language back, and its routines depend on it again.
    let mut db = build_db(&[("0001.sql", setup)]);
    db.apply_sql("DROP EXTENSION plpgsql CASCADE;").unwrap();
    assert!(db.find_functions(None, "pf").is_empty());
    assert!(db.find_functions(None, "pp").is_empty());
    assert_eq!(db.find_functions(None, "sf").len(), 1);
    db.apply_sql(
        "CREATE EXTENSION plpgsql;
         CREATE FUNCTION pf3() RETURNS int LANGUAGE plpgsql AS 'begin return 1; end';",
    )
    .unwrap();
    let err = db.apply_sql("DROP EXTENSION plpgsql;").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot drop extension plpgsql because other objects depend on it"),
        "{err}"
    );
}

#[test]
fn a_created_language_depends_on_its_handler_and_holds_its_routines() {
    let setup = "CREATE FUNCTION myh() RETURNS language_handler LANGUAGE c AS 'mylib';
                 CREATE LANGUAGE mylang HANDLER myh;
                 CREATE FUNCTION mf() RETURNS int LANGUAGE mylang AS 'x';";
    for (stmt, message) in [
        (
            "DROP FUNCTION myh();",
            "cannot drop function myh() because other objects depend on it (language mylang \
             depends on function myh())",
        ),
        (
            "DROP LANGUAGE mylang;",
            "cannot drop language mylang because other objects depend on it (function mf() \
             depends on language mylang)",
        ),
    ] {
        let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
        assert!(err.to_string().starts_with(message), "{stmt}\n  got: {err}");
    }
    let db = build_db(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "ALTER LANGUAGE mylang RENAME TO otherlang; DROP FUNCTION myh() CASCADE;",
        ),
    ]);
    assert!(db.find_functions(None, "mf").is_empty());
}
