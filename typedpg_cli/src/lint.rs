//! `cargo typedpg lint`: the same report as the TypeScript CLI's
//! `typedpg lint`, built from the `sql!` invocations of a Rust crate and
//! the migrations its `[package.metadata.typedpg]` config names.
//!
//! The migrations are collected exactly as the `sql!` macro collects them
//! (the configured directory's files are reported, the extra directories'
//! are applied but not reported), so the catalog a query is analyzed
//! against is the one it was compiled against.

mod scan;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use typedpg_analyzer::PgCatalog;
use typedpg_core::QualifiedName;
use typedpg_core::config::{Config, FINITE_PARAMETER_TYPES};
use typedpg_core::migrations::compare_migration_names;
use typedpg_lint::report::{Entry, Failure, Format, Report, position};
use typedpg_lint::{Finding, LintOptions, MigrationFile, lint_migrations, lint_query};

/// The directories under the manifest's directory that hold Rust sources.
const SOURCE_DIRS: &[&str] = &["src", "tests", "examples", "benches"];

/// One `sql!` site, with the file text its offsets are in.
struct Site {
    path: PathBuf,
    file: String,
    db: Option<String>,
    sql: String,
    literal: scan::Literal,
}

/// A query's findings, or its analysis error and offset.
type LintResult = Result<Vec<Finding>, (String, Option<usize>)>;

/// Lint `manifest`'s crate; whether no analysis failed and no finding
/// reaches `fail_on`. The report goes to stdout, the summary to stderr.
pub fn run(manifest: &Path, format: Format) -> Result<bool, String> {
    let config = Config::from_cargo_toml(manifest).map_err(|error| error.to_string())?;
    let resolved = config.resolve(None).map_err(|error| error.to_string())?;
    let options = LintOptions::from_config(&resolved.lint);
    let base = manifest_dir(manifest);
    let sites = scan_sources(&base)?;

    let mut report = Report {
        queries: sites.len(),
        ..Report::default()
    };
    let mut databases: Vec<Option<String>> = sites.iter().map(|site| site.db.clone()).collect();
    databases.sort();
    databases.dedup();

    let mut catalogs: HashMap<Option<String>, Result<PgCatalog, String>> = HashMap::new();
    for db in &databases {
        let catalog = match config.resolve(db.as_deref()) {
            Ok(_) => catalog_for(&config, db.as_deref(), &base, &options, &mut report),
            Err(error) => {
                let message = error.to_string();
                report.failures.push(Failure {
                    path: relative(manifest, &base),
                    start: None,
                    message: message.clone(),
                });
                Err(message)
            }
        };
        catalogs.insert(db.clone(), catalog);
    }

    let mut cache: HashMap<(Option<String>, String), LintResult> = HashMap::new();
    for site in &sites {
        let Ok(catalog) = catalogs.get(&site.db).expect("every database was built") else {
            continue;
        };
        let result = cache
            .entry((site.db.clone(), site.sql.clone()))
            .or_insert_with(|| {
                lint_query(catalog, &site.sql, &options)
                    .map_err(|error| (error.error.to_string(), error.offset))
            });
        let path = relative(&site.path, &base);
        match result {
            Ok(findings) => {
                for finding in findings {
                    report.entries.push(Entry {
                        path: path.clone(),
                        start: position(&site.file, site.literal.source_offset(finding.span.start)),
                        end: position(&site.file, site.literal.source_offset(finding.span.end)),
                        finding: finding.clone(),
                    });
                }
            }
            Err((message, offset)) => report.failures.push(Failure {
                path: path.clone(),
                start: offset
                    .map(|offset| position(&site.file, site.literal.source_offset(offset))),
                message: message.clone(),
            }),
        }
    }

    report.sort();
    let rendered = report.render(format);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    if format == Format::Human {
        for failure in &report.failures {
            eprintln!("{}", failure.render());
        }
    }
    eprintln!("{}", report.summary());
    Ok(!report.fails(&options))
}

/// The directory the manifest's paths resolve against, and the one output
/// paths are shown relative to.
fn manifest_dir(manifest: &Path) -> PathBuf {
    match manifest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `path` relative to `base`, with forward slashes.
fn relative(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn scan_sources(base: &Path) -> Result<Vec<Site>, String> {
    let mut paths = Vec::new();
    for dir in SOURCE_DIRS {
        collect_rust_files(&base.join(dir), &mut paths);
    }
    paths.sort();

    let mut sites = Vec::new();
    for path in paths {
        let file = match std::fs::read_to_string(&path) {
            Ok(file) => file,
            Err(error) => {
                eprintln!("typedpg lint: skipped {}: {error}", relative(&path, base));
                continue;
            }
        };
        let Some(queries) = scan::queries(&file) else {
            eprintln!(
                "typedpg lint: skipped {}: does not parse",
                relative(&path, base)
            );
            continue;
        };
        for query in queries {
            sites.push(Site {
                path: path.clone(),
                file: file.clone(),
                db: query.db,
                sql: query.text,
                literal: query.literal,
            });
        }
    }
    Ok(sites)
}

fn collect_rust_files(dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() != "target" {
                collect_rust_files(&path, paths);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.push(path);
        }
    }
}

/// Build the catalog of database `db` from its migrations, reporting the
/// failures that keep it from being built.
fn catalog_for(
    config: &Config,
    db: Option<&str>,
    base: &Path,
    options: &LintOptions,
    report: &mut Report,
) -> Result<PgCatalog, String> {
    let resolved = config.resolve(db).map_err(|error| error.to_string())?;
    let migrations_dir = resolved.migrations_dir(base);
    let extra_dirs = resolved.extra_migrations_dirs(base);
    let mut dirs = vec![(migrations_dir.clone(), true)];
    dirs.extend(extra_dirs.into_iter().map(|dir| (dir, false)));

    let files = match collect_migration_files(&dirs, base) {
        Ok(files) => files,
        Err(error) => {
            report.failures.push(Failure {
                path: relative(&migrations_dir, base),
                start: None,
                message: error.clone(),
            });
            return Err(error);
        }
    };
    report.migrations += files.len();
    let linted = match lint_migrations(&files, resolved.migrations_use_transaction(), options) {
        Ok(linted) => linted,
        Err(error) => {
            let message = error.to_string();
            report.failures.push(Failure {
                path: relative(&migrations_dir, base),
                start: None,
                message: message.clone(),
            });
            return Err(message);
        }
    };
    for migration in &linted.findings {
        let file = &files[migration.file];
        report.entries.push(Entry {
            path: file.path.clone(),
            start: position(&file.sql, migration.finding.span.start),
            end: position(&file.sql, migration.finding.span.end),
            finding: migration.finding.clone(),
        });
    }
    let mut catalog = linted.catalog;
    let finite: Vec<QualifiedName> = FINITE_PARAMETER_TYPES
        .iter()
        .map(|name| QualifiedName::new("pg_catalog", *name))
        .filter(|name| !resolved.types.contains_key(name))
        .collect();
    if let Err(error) = catalog.assume_finite_parameters(&finite) {
        report.failures.push(Failure {
            path: relative(&migrations_dir, base),
            start: None,
            message: error.clone(),
        });
        return Err(error);
    }
    Ok(catalog)
}

/// The database's migration files, in the order the macro applies them:
/// the runner's directory's files are reported, the extra directories'
/// are applied but not reported.
fn collect_migration_files(
    dirs: &[(PathBuf, bool)],
    base: &Path,
) -> Result<Vec<MigrationFile>, String> {
    let mut files: Vec<(OsString, String, String, bool)> = Vec::new();
    for (dir, lint) in dirs {
        if !dir.is_dir() {
            continue;
        }
        let entries = std::fs::read_dir(dir)
            .map_err(|error| format!("failed to read {}: {error}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.to_string_lossy();
            if !name.ends_with(".sql") || name.ends_with(".down.sql") {
                continue;
            }
            let sql = std::fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            let sql = typedpg_core::migration_marker::strip_bom(&sql).to_owned();
            files.push((entry.file_name(), relative(&path, base), sql, *lint));
        }
    }
    files.sort_by(|a, b| compare_migration_names(&a.0.to_string_lossy(), &b.0.to_string_lossy()));
    Ok(files
        .into_iter()
        .map(|(_, path, sql, lint)| MigrationFile { path, sql, lint })
        .collect())
}
