//! The catalog oracle over migration corpora and random DDL.
//!
//! With `pg_sanity` on, every migration a `PgCatalog` applies is mirrored on
//! a real PostgreSQL; with `PG_SANITY_CATALOG=1` (or
//! `PG_SANITY_CATALOG_LOG=<file>`) the mirror also compares the system
//! catalogs after each one (`typedpg_analyzer/src/pg_sanity/catalog_state.rs`).
//! The analyzer's own test suite exercises that on every test; these
//! tests feed it more DDL:
//!
//! - `catalog_oracle_corpus`: every directory under `CATALOG_CORPUS` is one
//!   project's migrations, applied in file-name order (`*.sql`) as the
//!   runner applies them (`-- no-transaction` ones statement by statement).
//! - `catalog_oracle_random_ddl`: `CATALOG_FUZZ_RUNS` (default 40) random
//!   sequences of `CATALOG_FUZZ_STEPS` (default 40) DDL statements, from
//!   `CATALOG_FUZZ_SEED` (default 0xCA7A).
//! - `extension_oracle`: every bundled extension the server image under test
//!   (`PG_IMAGE`; each registry entry names the image that ships it) has —
//!   each installable version, each single-step `ALTER EXTENSION ... UPDATE`
//!   (the default version reached by updates must be a fresh install of it,
//!   on the server too), a `SCHEMA` / `CASCADE` install, `DROP EXTENSION`
//!   back to nothing, and the control files' facts against
//!   `pg_available_extension_versions`. `EXTENSION_ORACLE_ONLY=a,b` limits it.
//!   The registry is covered by three images: `postgres:19.0`,
//!   `typedpg-pg-pl:19.0` (PL/Perl and PL/Python, for plperl, plperlu,
//!   plpython3u and their transforms: `scripts/pg-pl`, which
//!   `scripts/run-pg-sanity.sh` builds) and `typedpg-extensions:pg19` (the
//!   third-party ones: `scripts/build-extensions-image.sh`).
//! - `catalog_oracle_minimize`: the smallest subsequence of the migrations
//!   in `CATALOG_MINIMIZE` (separated by `;;` lines) that diverges the same
//!   way, greedily (run with `PG_SANITY_CATALOG=1` and no log).
//!
//! ```bash
//! PG_SANITY_CATALOG_LOG=/tmp/catalog.log CATALOG_CORPUS=/path/to/corpora \
//!   scripts/run-pg-sanity.sh --test catalog_oracle --run-ignored all
//! ```
//!
//! A run never stops at a divergence: each sequence runs under
//! `catch_unwind`, an outcome divergence is appended to the log too, and
//! the test fails at the end if anything was found — but for statements
//! both sides rejected with different errors, which random DDL often
//! faults several ways at once (`CATALOG_FUZZ_STRICT=1` fails those too).
#![cfg(feature = "pg_sanity")]

mod generator;

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};

use rand::SeedableRng;
use rand::rngs::StdRng;
use typedpg_analyzer::PgCatalog;

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| {
            s.strip_prefix("0x")
                .map_or_else(|| s.parse().ok(), |h| u64::from_str_radix(h, 16).ok())
        })
        .unwrap_or(default)
}

/// The size of the oracle's log: the catalog oracle appends a catalog's
/// divergence there without panicking.
fn log_size() -> u64 {
    std::env::var_os("PG_SANITY_CATALOG_LOG")
        .and_then(|path| std::fs::metadata(path).ok())
        .map_or(0, |m| m.len())
}

/// Append `record` to the oracle's log (or stderr without one).
fn log(record: &str) {
    match std::env::var_os("PG_SANITY_CATALOG_LOG") {
        Some(path) => {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("open the catalog oracle log");
            file.write_all(record.as_bytes())
                .expect("write the catalog oracle log");
        }
        None => eprintln!("{record}"),
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "<non-string panic>".into())
}

/// Run `steps` against a fresh catalog: a panic (an outcome divergence, or
/// the catalog oracle without a log) is logged and counted, with what
/// `steps` last put in its `at` (the statement or file it was applying).
fn run_sequence(label: &str, steps: impl FnOnce(&mut PgCatalog, &mut String)) -> bool {
    let mut at = String::new();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut catalog = PgCatalog::new().expect("catalog");
        steps(&mut catalog, &mut at);
    }));
    match result {
        Ok(()) => true,
        Err(payload) => {
            let message = panic_message(&*payload);
            // Both sides rejected the statement, with other errors: on a
            // statement with several faults PG's order is not a contract
            // (CLAUDE.md), and random DDL makes many such. Noted, not
            // failed, unless CATALOG_FUZZ_STRICT=1.
            let wording_only = message.starts_with("pg_sanity: apply_sql error must start with")
                || message.starts_with("pg_sanity: apply_sql SQLSTATE mismatch")
                || message.starts_with("pg_sanity: apply_sql error DETAIL");
            let fails = !wording_only || env_num("CATALOG_FUZZ_STRICT", 0) == 1;
            let kind = if fails { "panicked" } else { "both rejected" };
            log(&format!("=== {label} ({kind} at {at})\n{message}\n"));
            !fails
        }
    }
}

/// Apply migration file `name` as the migration runner does: in one
/// transaction, or — marked `-- no-transaction` — statement by statement.
fn apply_file(catalog: &mut PgCatalog, name: &str, sql: &str) -> Result<(), String> {
    if typedpg_core::migration_marker::has_no_transaction_marker(sql) {
        for statement in typedpg_core::migrations::split_statements(sql) {
            catalog
                .apply_migration(name, statement)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    } else {
        catalog
            .apply_migration(name, sql)
            .map_err(|e| e.to_string())
    }
}

#[test]
#[ignore = "catalog oracle over migration corpora: needs CATALOG_CORPUS and run-pg-sanity.sh"]
fn catalog_oracle_corpus() {
    let Some(root) = std::env::var_os("CATALOG_CORPUS") else {
        eprintln!("CATALOG_CORPUS is not set: nothing to run");
        return;
    };
    let mut projects: Vec<_> = std::fs::read_dir(&root)
        .expect("read CATALOG_CORPUS")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    projects.sort();
    let log_before = log_size();
    let mut failed = 0;
    for project in projects {
        let mut files: Vec<_> = std::fs::read_dir(&project)
            .expect("read a corpus project")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        files.sort();
        let label = project.display().to_string();
        eprintln!("{label}: {} migrations", files.len());
        let ok = run_sequence(&label, |catalog, at| {
            for file in &files {
                let sql = std::fs::read_to_string(file).expect("read a migration");
                let name = file.file_name().unwrap().to_string_lossy();
                at.clone_from(&name.to_string());
                if let Err(e) = apply_file(catalog, &name, &sql) {
                    // Both sides rejected it (the mirror compared the
                    // outcomes): the rest of the project builds on it.
                    log(&format!("=== {label}: {name} failed on both sides\n{e}\n"));
                    break;
                }
            }
        });
        failed += usize::from(!ok);
    }
    assert_eq!(
        failed, 0,
        "{failed} corpus project(s) diverged (see the log)"
    );
    assert_eq!(
        log_size(),
        log_before,
        "the catalogs diverged (see the log)"
    );
}

#[test]
#[ignore = "catalog oracle over random DDL: long-running, needs run-pg-sanity.sh"]
fn catalog_oracle_random_ddl() {
    let runs = env_num("CATALOG_FUZZ_RUNS", 40);
    let steps = env_num("CATALOG_FUZZ_STEPS", 40) as usize;
    let seed = env_num("CATALOG_FUZZ_SEED", 0xCA7A);
    let log_before = log_size();
    let mut failed = 0;
    for run in 0..runs {
        let mut rng = StdRng::seed_from_u64(seed.wrapping_add(run));
        let label = format!("random DDL, seed {:#x}", seed.wrapping_add(run));
        let ok = run_sequence(&label, |catalog, at| {
            let mut state = generator::State::default();
            for _ in 0..steps {
                let stmt = state.statement(&mut rng);
                at.push_str(&stmt);
                at.push_str(";\n");
                let ok = catalog.apply_sql(&stmt).is_ok();
                state.applied(&stmt, ok);
            }
        });
        failed += usize::from(!ok);
    }
    assert_eq!(
        failed, 0,
        "{failed} random sequence(s) diverged (see the log)"
    );
    assert_eq!(
        log_size(),
        log_before,
        "the catalogs diverged (see the log)"
    );
}

/// What a divergence is, to keep while minimizing: its first difference,
/// or the outcome divergence's kind and statement.
fn signature(message: &str) -> String {
    match message.split_once("--- differences:\n") {
        Some((_, diffs)) => diffs.lines().next().unwrap_or_default().to_owned(),
        None => message.lines().take(4).collect::<Vec<_>>().join("\n"),
    }
}

/// The divergence (its signature) `statements` run into, if any — under
/// `PG_SANITY_CATALOG=1`, where the catalog oracle panics.
fn diverges(statements: &[String]) -> Option<String> {
    catch_unwind(AssertUnwindSafe(|| {
        let mut catalog = PgCatalog::new().expect("catalog");
        for stmt in statements {
            let _ = catalog.apply_sql(stmt);
        }
    }))
    .err()
    .map(|payload| signature(&panic_message(&*payload)))
}

#[test]
#[ignore = "minimizes a diverging DDL sequence: needs CATALOG_MINIMIZE and run-pg-sanity.sh"]
fn catalog_oracle_minimize() {
    // CATALOG_MINIMIZE names a file of migrations separated by lines `;;`:
    // the smallest subsequence (greedily) that diverges the same way is
    // printed. Run it with PG_SANITY_CATALOG=1 and no log.
    let Some(path) = std::env::var_os("CATALOG_MINIMIZE") else {
        eprintln!("CATALOG_MINIMIZE is not set: nothing to minimize");
        return;
    };
    let text = std::fs::read_to_string(path).expect("read CATALOG_MINIMIZE");
    let mut statements: Vec<String> = text
        .split("\n;;\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let Some(target) = diverges(&statements) else {
        panic!("the sequence doesn't diverge");
    };
    let mut i = statements.len();
    while i > 0 {
        i -= 1;
        let mut fewer = statements.clone();
        fewer.remove(i);
        if diverges(&fewer).as_deref() == Some(target.as_str()) {
            statements = fewer;
        }
    }
    eprintln!(
        "minimized ({target}):\n{}",
        statements
            .iter()
            .map(|s| format!("{s};"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The versions of `ext` an install reaches (CREATE EXTENSION ... VERSION):
/// an install script's, and each one an update path leads to from one.
fn installable(ext: &typedpg_analyzer::ExtensionInfo) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = ext
        .versions
        .iter()
        .filter(|(_, from)| from.is_none())
        .map(|(v, _)| *v)
        .collect();
    let mut at = 0;
    while at < out.len() {
        let current = out[at];
        for (v, from) in &ext.versions {
            if *from == Some(current) && !out.contains(v) {
                out.push(v);
            }
        }
        at += 1;
    }
    out
}

/// The server catalog after `steps` on a fresh database, compared with the
/// analyzer's after each of them by the catalog oracle (a divergence
/// panics: `PG_SANITY_CATALOG=1`) — in `database` when given (see
/// [`PgCatalog::pg_sanity_database`]).
fn server_catalog_after(steps: &[String], database: Option<&str>) -> Option<Vec<String>> {
    let mut catalog = PgCatalog::new().expect("catalog");
    if let Some(database) = database {
        catalog.pg_sanity_database(database);
    }
    for step in steps {
        // Both sides reject it or neither (the mirror compares outcomes).
        let _ = catalog.apply_sql(step);
    }
    catalog.pg_sanity_server_catalog()
}

/// A row of `pg_available_extension_versions`.
struct AvailableVersion {
    name: String,
    version: String,
    superuser: bool,
    trusted: bool,
    relocatable: bool,
    schema: Option<String>,
    requires: Vec<String>,
}

#[test]
#[ignore = "extension oracle: needs run-pg-sanity.sh (PG_IMAGE names the image under test)"]
fn extension_oracle() {
    // Every bundled extension the server image under test ships: each
    // installable version, each single-step update, a SCHEMA and a
    // CASCADE install, DROP back to nothing, and the control files'
    // facts against pg_available_extension_versions.
    let image = std::env::var("PG_IMAGE").unwrap_or_else(|_| "postgres:19.0".into());
    let only: Option<Vec<String>> = std::env::var("EXTENSION_ORACLE_ONLY")
        .ok()
        .map(|s| s.split(',').map(str::to_owned).collect());
    let registry = typedpg_analyzer::extension_registry();
    let bundled = |n: &str| registry.iter().any(|e| e.name == n);
    let log_before = log_size();
    let mut failed = 0;
    let empty = server_catalog_after(&[], None).unwrap_or_default();

    // (5) The control files, as the server reads them.
    let url = std::env::var("POSTGRES_URL").expect("POSTGRES_URL");
    let mut client = typedpg_analyzer::pg_sanity_support::postgres::Client::connect(
        &url,
        typedpg_analyzer::pg_sanity_support::postgres::NoTls,
    )
    .expect("connect");
    let available: Vec<AvailableVersion> = client
        .query(
            "SELECT name::text, version, superuser, trusted, relocatable, schema::text, \
                    coalesce(requires::text[], '{}') \
             FROM pg_catalog.pg_available_extension_versions",
            &[],
        )
        .expect("pg_available_extension_versions")
        .iter()
        .map(|r| AvailableVersion {
            name: r.get(0),
            version: r.get(1),
            superuser: r.get(2),
            trusted: r.get(3),
            relocatable: r.get(4),
            schema: r.get(5),
            requires: r.get(6),
        })
        .collect();
    let defaults: Vec<(String, String)> = client
        .query(
            "SELECT name::text, default_version FROM pg_catalog.pg_available_extensions",
            &[],
        )
        .expect("pg_available_extensions")
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    for (name, _) in &defaults {
        if !bundled(name) && only.is_none() {
            log(&format!(
                "=== extension {name}: the server ({image}) ships it, the registry doesn't\n"
            ));
        }
    }

    for ext in &registry {
        if ext.image != image
            || only
                .as_ref()
                .is_some_and(|o| !o.iter().any(|n| n == ext.name))
        {
            continue;
        }
        let name = ext.name;
        let missing: Vec<&str> = ext
            .requires
            .iter()
            .copied()
            .filter(|r| !bundled(r))
            .collect();
        // The registry must bundle what its extensions require: no
        // extension is left out.
        if !missing.is_empty() {
            failed += 1;
            log(&format!(
                "=== extension {name}: requires {}, which the registry doesn't bundle\n",
                missing.join(", ")
            ));
            continue;
        }
        // (5)
        let server_default = defaults
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone());
        let mut facts = Vec::new();
        if server_default.as_deref() != Some(ext.default_version) {
            facts.push(format!(
                "default_version: {} in the registry, {server_default:?} in PostgreSQL",
                ext.default_version
            ));
        }
        let mut server_versions: Vec<&str> = available
            .iter()
            .filter(|a| a.name == name)
            .map(|a| a.version.as_str())
            .collect();
        server_versions.sort_unstable();
        let mut ours = installable(ext);
        ours.sort_unstable();
        if server_versions != ours {
            facts.push(format!(
                "versions: {ours:?} in the registry, {server_versions:?} in PostgreSQL"
            ));
        }
        for a in available.iter().filter(|a| a.name == name) {
            let mut requires: Vec<&str> = a.requires.iter().map(String::as_str).collect();
            requires.sort_unstable();
            let mut our_requires = ext.requires.to_vec();
            our_requires.sort_unstable();
            if a.relocatable != ext.relocatable
                || a.schema.as_deref() != ext.schema
                || requires != our_requires
                || a.superuser != ext.superuser
                || a.trusted != ext.trusted
            {
                facts.push(format!(
                    "version {}: relocatable / schema / requires / superuser / trusted are \
                     {} / {:?} / {:?} / {} / {} in the registry, {} / {:?} / {:?} / {} / {} in \
                     PostgreSQL",
                    a.version,
                    ext.relocatable,
                    ext.schema,
                    our_requires,
                    ext.superuser,
                    ext.trusted,
                    a.relocatable,
                    a.schema,
                    requires,
                    a.superuser,
                    a.trusted
                ));
            }
        }
        if !facts.is_empty() {
            failed += 1;
            log(&format!(
                "=== extension {name}: control file\n{}\n",
                facts.join("\n")
            ));
        }

        // pg_cron installs in `cron.database_name` only: the extensions
        // image's (scripts/extensions-image) is `pg_sanity_cron`.
        let database =
            (name == "pg_cron" || ext.requires.contains(&"pg_cron")).then_some("pg_sanity_cron");
        let cascade = if ext.requires.is_empty() {
            ""
        } else {
            " CASCADE"
        };
        let create = |version: &str, extra: &str| {
            format!("CREATE EXTENSION \"{name}\" VERSION '{version}'{extra}{cascade}")
        };
        // (1) Each installable version.
        let mut fresh_default = None;
        for version in installable(ext) {
            let steps = vec![create(version, "")];
            let mut dump = None;
            let ok = run_sequence(&format!("extension {name} {version}"), |_, at| {
                at.clone_from(&steps[0]);
                dump = server_catalog_after(&steps, database);
            });
            failed += usize::from(!ok);
            if version == ext.default_version {
                fresh_default = dump;
            }
        }
        // (2) Each single-step update, and the default version reached by
        // updates against a fresh install of it — on the server too.
        for (to, from) in &ext.versions {
            let Some(from) = from else { continue };
            let steps = vec![
                create(from, ""),
                format!("ALTER EXTENSION \"{name}\" UPDATE TO '{to}'"),
            ];
            let mut dump = None;
            let ok = run_sequence(&format!("extension {name} {from} -> {to}"), |_, at| {
                at.clone_from(&steps.join(";\n"));
                dump = server_catalog_after(&steps, database);
            });
            failed += usize::from(!ok);
            if *to == ext.default_version
                && let (Some(updated), Some(fresh)) = (&dump, &fresh_default)
                && updated != fresh
            {
                failed += 1;
                let only_updated: Vec<&String> = updated
                    .iter()
                    .filter(|l| !fresh.contains(l))
                    .take(20)
                    .collect();
                let only_fresh: Vec<&String> = fresh
                    .iter()
                    .filter(|l| !updated.contains(l))
                    .take(20)
                    .collect();
                log(&format!(
                    "=== extension {name}: {from} -> {to} isn't a fresh {to} on the server\n\
                     only updated: {only_updated:#?}\nonly fresh: {only_fresh:#?}\n"
                ));
            }
        }
        // (3) Into another schema, and with CASCADE (its requirements).
        let steps = vec![
            "CREATE SCHEMA other_schema".to_owned(),
            create(ext.default_version, " SCHEMA other_schema"),
        ];
        let ok = run_sequence(&format!("extension {name} SCHEMA"), |_, at| {
            at.clone_from(&steps.join(";\n"));
            server_catalog_after(&steps, database);
        });
        failed += usize::from(!ok);
        // (4) DROP EXTENSION returns to nothing.
        let steps = vec![
            create(ext.default_version, ""),
            format!("DROP EXTENSION \"{name}\" CASCADE"),
        ];
        let mut dump = None;
        let ok = run_sequence(&format!("extension {name} DROP"), |_, at| {
            at.clone_from(&steps.join(";\n"));
            dump = server_catalog_after(&steps, database);
        });
        failed += usize::from(!ok);
        // What CASCADE installed stays; so does the control file's fixed
        // schema, which CREATE EXTENSION created outside the extension
        // (CreateExtensionInternal: not a member, DROP leaves it).
        let required_left = |l: &String| {
            ext.requires
                .iter()
                .any(|r| l.contains(&format!("pg_extension {r} ")))
                || ext
                    .schema
                    .is_some_and(|schema| l.starts_with(&format!("pg_namespace {schema} ")))
        };
        if let Some(dump) = dump {
            let left: Vec<&String> = dump
                .iter()
                .filter(|l| !empty.contains(l) && !required_left(l))
                .collect();
            if !left.is_empty() && ext.requires.is_empty() {
                failed += 1;
                log(&format!(
                    "=== extension {name}: DROP EXTENSION left rows on the server\n{left:#?}\n"
                ));
            }
        }
    }
    assert_eq!(
        failed, 0,
        "{failed} extension check(s) diverged (see the log)"
    );
    assert_eq!(
        log_size(),
        log_before,
        "the catalogs diverged (see the log)"
    );
}
