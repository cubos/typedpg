//! Generates `seed.json` for the static analyzer from a live PostgreSQL instance.
//!
//! Usage:
//!   cargo run -p typedpg_seed
//!
//! Spins up a disposable `postgres:<PG_VERSION>` container (the release
//! typedpg targets, `typedpg_pg_query/POSTGRES_VERSION`) via the Docker daemon
//! (using `testcontainers`), waits for it to accept connections, exports each
//! `pg_catalog` table almost 1:1 into the analyzer's `PgCatalogSeed`, then
//! stops + removes the container (via `Drop`). The output is written to
//! `typedpg_analyzer/src/seed.json`.

use testcontainers::GenericImage;
use testcontainers::ImageExt;
use testcontainers::core::Mount;
use testcontainers::core::WaitFor;
use testcontainers::runners::SyncRunner;
use typedpg_analyzer::{PgCatalog, PgCatalogSeed, PgTypeOid, QualifiedName};

fn main() {
    // The PostgreSQL release typedpg targets: the seed must describe the
    // same server the parser speaks for.
    let tag = typedpg_pg_query::PG_VERSION;
    eprintln!("Pulling postgres:{tag} from registry...");
    // The official image, as testcontainers-modules' `Postgres` configured
    // it: user / password / database `postgres`, fsync off, ready once the
    // server logs that it accepts connections.
    let request = GenericImage::new("postgres", tag)
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_wait_for(WaitFor::message_on_stdout(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_USER", "postgres")
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_env_var("POSTGRES_DB", "postgres")
        .with_cmd(["-c", "fsync=off"])
        .with_mount(Mount::tmpfs_mount("/var/lib/postgresql"))
        .pull_image()
        .unwrap_or_else(|e| panic!("failed to pull postgres:{tag}: {e}"));

    eprintln!("Starting postgres:{tag} container...");
    let container = request.start().expect("failed to start postgres container");

    let host = container.get_host().expect("failed to get container host");
    let port = container
        .get_host_port_ipv4(5432)
        .expect("failed to get container port");
    let conn_str =
        format!("host={host} port={port} user=postgres password=postgres dbname=postgres");

    eprintln!("Connecting to: {conn_str}");
    let mut client = postgres::Client::connect(&conn_str, postgres::NoTls)
        .expect("failed to connect to PostgreSQL");

    eprintln!("Exporting catalog...");
    let snapshot = typedpg_analyzer::catalog_export::export_catalog(&mut client)
        .expect("failed to export schema");

    eprintln!("Exporting view definitions...");
    let view_defs =
        export_view_definitions(&mut client).expect("failed to export view definitions");
    eprintln!(
        "Populating pg_rewrite._RETURN for {} view(s)...",
        view_defs.len()
    );
    let snapshot = populate_view_defs(snapshot, view_defs);

    let json = serde_json::to_string(&snapshot).expect("failed to serialize snapshot");

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out_path = std::path::Path::new(manifest_dir)
        .parent()
        .unwrap()
        .join("typedpg_analyzer/src/seed.json");
    std::fs::write(&out_path, &json).expect("failed to write seed.json");

    let size_kb = json.len() / 1024;
    eprintln!("Wrote {out_path:?} ({size_kb} KB)");
    eprintln!("  pg_namespace: {}", snapshot.pg_namespace.len());
    eprintln!("  pg_type:      {}", snapshot.pg_type.len());
    eprintln!("  pg_enum:      {}", snapshot.pg_enum.len());
    eprintln!("  pg_range:     {}", snapshot.pg_range.len());
    eprintln!("  pg_class:     {}", snapshot.pg_class.len());
    eprintln!("  pg_attribute: {}", snapshot.pg_attribute.len());
    eprintln!("  pg_proc:      {}", snapshot.pg_proc.len());
    eprintln!("  pg_aggregate: {}", snapshot.pg_aggregate.len());
    eprintln!("  pg_operator:  {}", snapshot.pg_operator.len());
    eprintln!("  pg_cast:      {}", snapshot.pg_cast.len());
    eprintln!("  pg_extension: {}", snapshot.pg_extension.len());
    eprintln!("  pg_language:  {}", snapshot.pg_language.len());
    eprintln!("  pg_depend:    {}", snapshot.pg_depend.len());
    eprintln!("  pg_inherits:  {}", snapshot.pg_inherits.len());
    eprintln!("  pg_constraint:{}", snapshot.pg_constraint.len());
    eprintln!("  pg_index:     {}", snapshot.pg_index.len());
    eprintln!("  pg_rewrite:   {}", snapshot.pg_rewrite.len());
    eprintln!("  pg_acl:       {}", snapshot.pg_acl.len());
}

fn export_view_definitions(
    client: &mut postgres::Client,
) -> Result<Vec<(String, String, String)>, postgres::Error> {
    let rows = client.query(
        "SELECT schemaname, viewname, definition FROM pg_catalog.pg_views \
         UNION ALL \
         SELECT schemaname, matviewname, definition FROM pg_catalog.pg_matviews \
         ORDER BY 1, 2",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let schema: String = r.get(0);
            let name: String = r.get(1);
            let definition: String = r.get(2);
            (schema, name, definition)
        })
        .collect())
}

/// Re-apply each view's `CREATE VIEW` through the analyzer's DDL pipeline so
/// the matching `pg_rewrite._RETURN` row gets emitted. Continues to use
/// `to_seed()`/`from_seed()` for round-trip.
///
/// Views the analyzer can't yet handle (typically polymorphic /
/// information_schema oddities) are logged and skipped — pg_rewrite stays
/// empty for those, but the rest of the seed is still produced.
fn populate_view_defs(seed: PgCatalogSeed, defs: Vec<(String, String, String)>) -> PgCatalogSeed {
    let mut db = PgCatalog::from_seed(seed);
    let mut failed = Vec::new();

    for (schema, name, definition) in &defs {
        let qn = QualifiedName::new(schema.clone(), name.clone());

        let before: Vec<(String, PgTypeOid)> = db
            .resolve_table(Some(schema), name)
            .map(|c| {
                db.attributes_of(c.oid)
                    .iter()
                    .map(|a| (a.attname.clone(), a.atttypid))
                    .collect()
            })
            .unwrap_or_default();

        let sql = format!(
            "CREATE OR REPLACE VIEW {qn} AS {body}",
            qn = qn,
            body = definition.trim_end().trim_end_matches(';'),
        );

        if let Err(err) = db.apply_sql(&sql) {
            log_view_failure(&qn, &err.to_string(), &sql);
            failed.push(qn.to_string());
            continue;
        }

        let after: Vec<(String, PgTypeOid)> = db
            .resolve_table(Some(schema), name)
            .map(|c| {
                db.attributes_of(c.oid)
                    .iter()
                    .map(|a| (a.attname.clone(), a.atttypid))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(drift) = describe_column_drift(&before, &after) {
            log_view_failure(&qn, &format!("column drift: {drift}"), &sql);
            failed.push(qn.to_string());
        }
    }

    if !failed.is_empty() {
        eprintln!(
            "\n{} view(s) failed to reanalyze (see errors above):",
            failed.len()
        );
        for s in &failed {
            eprintln!("  - {s}");
        }
        panic!(
            "refusing to emit incomplete seed.json: {} view(s) did not round-trip through the analyzer",
            failed.len()
        );
    }

    db.to_seed()
}

fn log_view_failure(qn: &QualifiedName, error: &str, sql: &str) {
    eprintln!("--- {qn} ---");
    eprintln!("  error: {error}");
    eprintln!("  sql: {sql}");
}

fn describe_column_drift(
    before: &[(String, PgTypeOid)],
    after: &[(String, PgTypeOid)],
) -> Option<String> {
    if before.len() != after.len() {
        return Some(format!("column count {} → {}", before.len(), after.len()));
    }
    for (i, (b, a)) in before.iter().zip(after.iter()).enumerate() {
        if b.0 != a.0 {
            return Some(format!("column #{i} name {:?} → {:?}", b.0, a.0));
        }
        if b.1 != a.1 {
            return Some(format!(
                "column #{i} type_oid {} → {}",
                b.1.get(),
                a.1.get()
            ));
        }
    }
    None
}
