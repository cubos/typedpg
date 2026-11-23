//! The third-party extensions bundled besides pgvector: PostGIS, pg_cron,
//! pgmq and pg_jsonschema. Under `pg_sanity` every migration and query here
//! is mirrored on the server; on an image without the extension (the default
//! `postgres` one) the mirror stops comparing once the server reports it as
//! not available. `scripts/build-extensions-image.sh` builds the image that
//! carries them all (`PG_IMAGE=typedpg-extensions:pg19
//! scripts/run-pg-sanity.sh third_party`).

use crate::common::*;

/// A type as a cast names it, without `pg_catalog.` and quotes.
fn short(t: &Type) -> String {
    let name = t.cast_name().unwrap_or_default().replace('"', "");
    name.strip_prefix("pg_catalog.").unwrap_or(&name).to_owned()
}

fn param_types(db: &PgCatalog, sql: &str) -> Vec<String> {
    db.analyze(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .params
        .iter()
        .map(|p| short(&p.pg_type))
        .collect()
}

fn column_types(db: &PgCatalog, sql: &str) -> Vec<(String, bool)> {
    db.analyze(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .columns
        .iter()
        .map(|c| (short(&c.pg_type), c.nullable))
        .collect()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

// ── PostGIS ─────────────────────────────────────────────────────────────────

const PLACES: &str = "CREATE EXTENSION postgis;
    CREATE TABLE places (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        name text NOT NULL,
        geom geometry(Point, 4326) NOT NULL CHECK (ST_SRID(geom) = 4326),
        geog geography(Point),
        area geometry(PolygonZ, 3857) CHECK (ST_IsValid(area)),
        anything geometry
    );
    CREATE INDEX places_geom ON places USING gist (geom);
    CREATE INDEX places_geog ON places USING gist (geog);";

#[test]
fn third_party_postgis_installs() {
    let db = build(&[("0001.sql", "CREATE EXTENSION postgis;")]);
    for name in ["geometry", "geography", "box2d", "box3d"] {
        let t = db.resolve_type_by_name(Some("public"), name).unwrap();
        assert_eq!(db.extension_of_type(t.oid), Some("postgis"), "{name}");
    }
    assert!(!db.find_functions(Some("public"), "st_dwithin").is_empty());
}

#[test]
fn third_party_postgis_columns_take_their_typmod() {
    // geometry_typmod_in / geography_typmod_in: the type, then the SRID; a
    // geography's defaults to 4326.
    let db = build_db(&[("0001.sql", PLACES)]);
    let cols = db
        .analyze("SELECT geom, geog, area, anything FROM places")
        .unwrap()
        .columns;
    let typmods: Vec<Option<i32>> = cols
        .iter()
        .map(|c| match &c.pg_type {
            Type::Basic { typmod, .. } => *typmod,
            other => panic!("{other:?}"),
        })
        .collect();
    // (4326 << 8) | (POINTTYPE << 2); (3857 << 8) | (POLYGONTYPE << 2) | Z.
    assert_eq!(
        typmods,
        [Some(1_107_460), Some(1_107_460), Some(987_406), None]
    );
}

#[test]
fn third_party_postgis_typmod_rules() {
    // What PostgreSQL stores (and format_type prints) for each modifier,
    // and what geometry_typmod_in / geography_typmod_in reject.
    for (column, ok) in [
        ("geometry(point)", true),
        ("geometry(' pointzm ')", true),
        ("geometry(Geometry)", true),
        ("geometry(GeometryM, 4326)", true),
        ("geometry(Point, 4326, 1)", true),
        ("geometry(Point, -5)", true),
        ("geometry(Point, 1000000)", true),
        ("geometry(Point, ' 12')", true),
        ("geography(Point, 0)", true),
        ("geography(Point, 4269)", true),
        ("geography(TinZM)", true),
        ("geometry(Foo)", false),
        ("geometry(4326)", false),
        ("geometry(Point, 'x')", false),
        ("geometry(Point, '12 ')", false),
        ("geometry(Point, 99999999999)", false),
        ("geometry(Point, 2.5)", false),
        ("geography(Point, 3857)", false),
        ("geography(Point, 1000000)", false),
        ("geography(Point, 999070)", false),
        ("geography(Point, 999283)", false),
    ] {
        let result = try_apply(&[
            ("0001.sql", "CREATE EXTENSION postgis;"),
            ("0002.sql", &format!("CREATE TABLE t (g {column});")),
        ]);
        assert_eq!(result.is_ok(), ok, "{column}: {result:?}");
    }
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE EXTENSION postgis; CREATE TABLE t (g geometry(Foo));"
        )]),
        DdlError::UnsupportedDdl(_),
        "Invalid geometry type modifier: foo",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE EXTENSION postgis; CREATE TABLE t (g geography(Point, 3857));"
        )]),
        DdlError::UnsupportedDdl(_),
        "Only lon/lat coordinate systems are supported in geography.",
    );
}

#[test]
fn third_party_postgis_queries() {
    let db = build_db(&[("0001.sql", PLACES)]);
    assert_eq!(
        param_types(
            &db,
            "SELECT id FROM places WHERE ST_DWithin(geog, ST_MakePoint($1, $2)::geography, $3)"
        ),
        s(&["float8", "float8", "float8"])
    );
    assert_eq!(
        column_types(
            &db,
            "SELECT ST_AsText(geom) AS wkt, ST_X(geom) AS x, ST_Distance(geog, geog) AS d, \
                    geom FROM places"
        ),
        [
            ("text".to_owned(), true),
            ("float8".to_owned(), true),
            ("float8".to_owned(), true),
            ("public.geometry".to_owned(), false),
        ]
    );
    assert_eq!(
        param_types(
            &db,
            "SELECT name FROM places \
             ORDER BY geom <-> ST_SetSRID(ST_MakePoint($1, $2), 4326) LIMIT $3"
        ),
        s(&["float8", "float8", "int8"])
    );
    assert_eq!(
        param_types(
            &db,
            "SELECT id FROM places WHERE geom && ST_MakeEnvelope($1, $2, $3, $4, 4326)"
        ),
        s(&["float8", "float8", "float8", "float8"])
    );
    assert_eq!(
        param_types(
            &db,
            "INSERT INTO places (name, geom) VALUES ($1, ST_GeomFromText($2, 4326))"
        ),
        s(&["text", "text"])
    );
}

#[test]
fn third_party_postgis_raster_installs() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION postgis_raster CASCADE;
         CREATE TABLE tiles (id int PRIMARY KEY, rast raster NOT NULL);",
    )]);
    assert_eq!(
        column_types(&db, "SELECT ST_Width(rast) AS w FROM tiles"),
        [("int4".to_owned(), true)]
    );
}

#[test]
fn third_party_postgis_topology_installs() {
    // Its script adds `topology` to the database's search path (and the
    // session's): its objects resolve unqualified afterwards.
    let db = build_db(&[
        ("0001.sql", "CREATE EXTENSION postgis_topology CASCADE;"),
        (
            "0002.sql",
            "CREATE TABLE parcels (id int PRIMARY KEY, topo topogeometry);",
        ),
    ]);
    assert_eq!(
        column_types(&db, "SELECT name FROM topology"),
        [("varchar".to_owned(), false)]
    );
    assert_eq!(
        column_types(&db, "SELECT topology.CreateTopology('t', 4326) AS id"),
        [("int4".to_owned(), true)]
    );
}

#[test]
fn third_party_postgis_topology_installs_in_its_schema() {
    // The control file's schema: created when missing, and no other one
    // may be given — and one given must exist, the control file's too.
    build(&[(
        "0001.sql",
        "CREATE EXTENSION postgis; CREATE SCHEMA topology;
         CREATE EXTENSION postgis_topology SCHEMA topology;",
    )]);
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE EXTENSION postgis; CREATE EXTENSION postgis_topology SCHEMA topology;",
        )]),
        DdlError::TableNotFound(_),
        "schema \"topology\" does not exist",
    );
    assert_ddl_err!(
        try_apply(&[(
            "0001.sql",
            "CREATE EXTENSION postgis; CREATE EXTENSION postgis_topology SCHEMA public;",
        )]),
        DdlError::ExtensionError(_),
        "extension \"postgis_topology\" must be installed in schema \"topology\"",
    );
}

#[test]
fn third_party_postgis_sfcgal_installs() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE EXTENSION postgis_sfcgal CASCADE;
         CREATE TABLE solids (id int PRIMARY KEY, g geometry NOT NULL);",
    )]);
    assert_eq!(
        column_types(&db, "SELECT CG_Area(g) AS a FROM solids"),
        [("float8".to_owned(), true)]
    );
}

#[test]
fn third_party_postgis_requires_postgis() {
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE EXTENSION postgis_raster;")]),
        DdlError::TypeNotFound(_),
        "required extension \"postgis\" is not installed (Use CREATE EXTENSION ... CASCADE to \
         install required extensions too.)",
    );
}

// ── pg_cron ─────────────────────────────────────────────────────────────────

/// A catalog whose mirror runs in `cron.database_name` (the extensions
/// image's), the only database pg_cron installs in.
fn cron_db(migrations: &[&str]) -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.pg_sanity_database("pg_sanity_cron");
    for sql in migrations {
        db.apply_sql(sql).unwrap();
    }
    db
}

#[test]
fn third_party_pg_cron_installs() {
    let db = cron_db(&["CREATE EXTENSION pg_cron;"]);
    assert_eq!(
        column_types(
            &db,
            "SELECT cron.schedule('nightly', '0 3 * * *', 'VACUUM') AS id"
        ),
        [("int8".to_owned(), true)]
    );
    assert_eq!(
        column_types(
            &db,
            "SELECT jobid, schedule, command, jobname, active FROM cron.job"
        ),
        [
            ("int8".to_owned(), false),
            ("text".to_owned(), false),
            ("text".to_owned(), false),
            ("text".to_owned(), true),
            ("bool".to_owned(), false),
        ]
    );
    assert_eq!(
        param_types(&db, "SELECT cron.unschedule($1::bigint) AS ok"),
        s(&["int8"])
    );
    assert_eq!(
        param_types(
            &db,
            "SELECT status, return_message FROM cron.job_run_details WHERE jobid = $1"
        ),
        s(&["int8"])
    );
}

#[test]
fn third_party_pg_cron_updates_along_its_chain() {
    // 1.4's schedule(name, ...) became schedule(text, ...), its jobname a
    // text: the update scripts run on the 1.4 installation.
    let db = cron_db(&[
        "CREATE EXTENSION pg_cron VERSION '1.4';",
        "ALTER EXTENSION pg_cron UPDATE;",
    ]);
    assert_eq!(
        column_types(&db, "SELECT jobname FROM cron.job"),
        [("text".to_owned(), true)]
    );
}

#[test]
fn third_party_pg_cron_schema_goes_with_it() {
    // The `cron` schema its script creates is a member: DROP EXTENSION
    // drops it, with its tables.
    let db = cron_db(&["CREATE EXTENSION pg_cron;", "DROP EXTENSION pg_cron;"]);
    assert!(db.resolve_table(Some("cron"), "job").is_none());
    assert!(db.namespace_oid("cron").is_none());
}

#[test]
fn third_party_pg_cron_schema_holding_other_objects() {
    // Something else in the member schema depends on it: CASCADE drops it.
    let mut db = PgCatalog::new().unwrap();
    db.pg_sanity_database("pg_sanity_cron");
    db.apply_sql("CREATE EXTENSION pg_cron; CREATE TABLE cron.mine (x int);")
        .unwrap();
    let err = db.apply_sql("DROP EXTENSION pg_cron;").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot drop extension pg_cron because other objects depend on it"),
        "{err}"
    );
    db.apply_sql("DROP EXTENSION pg_cron CASCADE;").unwrap();
    assert!(db.resolve_table(Some("cron"), "mine").is_none());
}

#[test]
fn third_party_pg_cron_installs_in_pg_catalog() {
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE EXTENSION pg_cron SCHEMA public;")]),
        DdlError::ExtensionError(_),
        "extension \"pg_cron\" must be installed in schema \"pg_catalog\"",
    );
}

// ── pgmq ────────────────────────────────────────────────────────────────────

#[test]
fn third_party_pgmq_installs() {
    let db = build_db(&[("0001.sql", "CREATE EXTENSION pgmq;")]);
    assert_eq!(
        param_types(&db, "SELECT pgmq.send('orders', $1::jsonb) AS id"),
        s(&["jsonb"])
    );
    assert_eq!(
        column_types(&db, "SELECT pgmq.send('orders', '{}'::jsonb) AS id"),
        [("int8".to_owned(), true)]
    );
    assert_eq!(
        column_types(
            &db,
            "SELECT msg_id, read_ct, message FROM pgmq.read('orders', 30, $1)"
        ),
        [
            ("int8".to_owned(), true),
            ("int4".to_owned(), true),
            ("jsonb".to_owned(), true),
        ]
    );
    assert_eq!(
        param_types(&db, "SELECT pgmq.archive('orders', $1::bigint) AS done"),
        s(&["int8"])
    );
}

#[test]
fn third_party_pgmq_queue_in_a_migration() {
    // pgmq.create builds the queue's tables with EXECUTE format(...) of
    // the table names pgmq.format_table_name computes: the analyzer runs
    // the function's body, and the nested functions' it calls.
    let db = build_db(&[
        ("0001.sql", "CREATE EXTENSION pgmq;"),
        ("0002.sql", "SELECT pgmq.create('orders');"),
    ]);
    assert_eq!(
        column_types(&db, "SELECT msg_id, message, vt FROM pgmq.q_orders"),
        [
            ("int8".to_owned(), false),
            ("jsonb".to_owned(), true),
            ("timestamptz".to_owned(), false),
        ]
    );
    assert_eq!(
        column_types(&db, "SELECT archived_at FROM pgmq.a_orders"),
        [("timestamptz".to_owned(), false)]
    );
}

#[test]
fn third_party_pgmq_installs_its_release_version_only() {
    // A pgmq release ships one installation script, its own version's.
    assert_ddl_err!(
        try_apply(&[("0001.sql", "CREATE EXTENSION pgmq VERSION '1.13.0';")]),
        DdlError::ExtensionError(_),
        "extension \"pgmq\" has no installation script nor update path for version \"1.13.0\"",
    );
}

// ── pg_jsonschema ───────────────────────────────────────────────────────────

#[test]
fn third_party_pg_jsonschema_installs() {
    let db = build_db(&[(
        "0001.sql",
        r#"CREATE EXTENSION pg_jsonschema;
           CREATE TABLE docs (
               id int PRIMARY KEY,
               body jsonb NOT NULL CHECK (jsonb_matches_schema(
                   '{"type": "object", "required": ["title"]}', body)),
               meta json CHECK (json_matches_schema('{"type": "object"}', meta))
           );"#,
    )]);
    assert_eq!(
        param_types(&db, "SELECT json_matches_schema($1, $2) AS ok"),
        s(&["json", "json"])
    );
    assert_eq!(
        column_types(
            &db,
            "SELECT jsonschema_validation_errors('{\"type\": \"object\"}', meta) AS errors, \
                    jsonschema_is_valid('{}') AS valid FROM docs"
        ),
        [("text[]".to_owned(), true), ("bool".to_owned(), true)]
    );
    assert_eq!(
        param_types(&db, "INSERT INTO docs (id, body) VALUES ($1, $2)"),
        s(&["int4", "jsonb"])
    );
}
