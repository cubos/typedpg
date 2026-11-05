//! `cargo typedpg lint`: the same report as the TypeScript CLI's
//! `typedpg lint`, built from the `sql!` and `copy_in!` invocations of a
//! Rust crate and the migrations its `[package.metadata.typedpg]` config
//! names.
//!
//! The migrations are collected exactly as the `sql!` macro collects them
//! (the configured directory's files are reported, the extra directories'
//! are applied but not reported), so the catalog a query is analyzed
//! against is the one it was compiled against. Every configured database is
//! linted, whether or not a query names it.

mod scan;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

use typedpg_analyzer::PgCatalog;
use typedpg_core::QualifiedName;
use typedpg_core::config::{Config, FINITE_PARAMETER_TYPES};
use typedpg_core::migrations::compare_migration_names;
use typedpg_lint::report::{
    Entry, Failure, Format, Position, Report, position, stderr_colors, stdout_colors,
};
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

/// One `copy_in!` target, with the file text its offsets are in.
struct CopySite {
    path: PathBuf,
    file: String,
    db: Option<String>,
    target: String,
    literal: scan::Literal,
}

/// What the sources hold, and the failures reading them already found.
#[derive(Default)]
struct Sources {
    sites: Vec<Site>,
    copies: Vec<CopySite>,
    failures: Vec<Failure>,
}

/// A statement's analysis error: its message and byte offset.
type AnalysisError = (String, Option<usize>);

/// A statement's findings or error.
type LintResult = Result<Vec<Finding>, AnalysisError>;

/// A COPY target's analysis outcome.
type CopyResult = Result<(), AnalysisError>;

/// Lint `manifest`'s crate; whether no analysis failed and no finding
/// reaches `fail_on`. The report goes to stdout, the summary to stderr.
pub fn run(manifest: &Path, format: Format) -> Result<bool, String> {
    let start = Instant::now();
    let base = manifest_dir(manifest);
    let config = match Config::from_cargo_toml(manifest) {
        Ok(config) => config,
        Err(error) => return finish_with_config_error(error, manifest, &base, start, format),
    };
    let options = match config.resolve(None) {
        Ok(resolved) => LintOptions::from_config(&resolved.lint),
        Err(error) => return finish_with_config_error(error, manifest, &base, start, format),
    };
    let Sources {
        sites,
        copies,
        failures,
    } = scan_sources(&base);
    let mut report = Report {
        queries: sites.len() + copies.len(),
        failures,
        ..Report::default()
    };
    // The default database, then every configured one: a named database's
    // migrations are linted even when no query names it.
    let mut databases: Vec<Option<String>> = vec![None];
    databases.extend(config.database_names().into_iter().map(Some));

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
        if let Ok(catalog) = &catalog {
            report.schemas.push((
                db.clone().unwrap_or_else(|| "default".to_owned()),
                catalog.user_object_counts(),
            ));
        }
        catalogs.insert(db.clone(), catalog);
    }

    let mut cache: HashMap<(Option<String>, String), LintResult> = HashMap::new();
    for site in &sites {
        let path = relative(&site.path, &base);
        report.sources.insert(path.clone(), site.file.clone());
        let Ok(catalog) = catalogs.get(&site.db).expect("every database was built") else {
            continue;
        };
        let result = cache
            .entry((site.db.clone(), site.sql.clone()))
            .or_insert_with(|| {
                lint_query(catalog, &site.sql, &options)
                    .map_err(|error| (error.error.to_string(), error.offset))
            });
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
                start: Some(
                    offset
                        .map(|offset| position(&site.file, site.literal.source_offset(offset)))
                        .unwrap_or_else(|| position(&site.file, site.literal.source_offset(0))),
                ),
                message: message.clone(),
            }),
        }
    }

    let mut copy_cache: HashMap<(Option<String>, String), CopyResult> = HashMap::new();
    for copy in &copies {
        let path = relative(&copy.path, &base);
        report.sources.insert(path.clone(), copy.file.clone());
        let Ok(catalog) = catalogs.get(&copy.db).expect("every database was built") else {
            continue;
        };
        let result = copy_cache
            .entry((copy.db.clone(), copy.target.clone()))
            .or_insert_with(|| {
                catalog
                    .analyze_copy_in_located(&copy.target)
                    .map(|_| ())
                    .map_err(|error| (error.error.to_string(), error.offset))
            });
        if let Err((message, offset)) = result {
            report.failures.push(Failure {
                path: path.clone(),
                start: Some(
                    offset
                        .map(|offset| position(&copy.file, copy.literal.source_offset(offset)))
                        .unwrap_or_else(|| position(&copy.file, copy.literal.source_offset(0))),
                ),
                message: message.clone(),
            });
        }
    }

    report.load_failure_sources(&base);
    report.sort();
    report.elapsed = start.elapsed();
    Ok(finish(&report, &options, format))
}

/// The report of a configuration that could not be read: one failure.
fn finish_with_config_error(
    error: impl std::fmt::Display,
    manifest: &Path,
    base: &Path,
    start: Instant,
    format: Format,
) -> Result<bool, String> {
    let options = LintOptions::default();
    let mut report = Report {
        elapsed: start.elapsed(),
        ..Report::default()
    };
    report.failures.push(Failure {
        path: relative(manifest, base),
        start: None,
        message: error.to_string(),
    });
    Ok(finish(&report, &options, format))
}

fn finish(report: &Report, options: &LintOptions, format: Format) -> bool {
    let rendered = report.render_with(format, options, stdout_colors());
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    eprintln!("{}", report.summary_with(options, stderr_colors()));
    !report.fails(options)
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

fn scan_sources(base: &Path) -> Sources {
    let mut paths = Vec::new();
    for dir in SOURCE_DIRS {
        collect_rust_files(&base.join(dir), &mut paths);
    }
    paths.sort();

    let mut sources = Sources::default();
    for path in paths {
        let shown = relative(&path, base);
        let file = match std::fs::read_to_string(&path) {
            Ok(file) => file,
            Err(error) => {
                sources.failures.push(Failure {
                    path: shown,
                    start: None,
                    message: error.to_string(),
                });
                continue;
            }
        };
        let scanned = match scan::scan(&file) {
            Ok(scanned) => scanned,
            Err(error) => {
                let start = error.span().start();
                sources.failures.push(Failure {
                    path: shown,
                    start: Some(Position {
                        line: start.line as u32,
                        column: start.column as u32 + 1,
                    }),
                    message: error.to_string(),
                });
                continue;
            }
        };
        for error in scanned.errors {
            sources.failures.push(Failure {
                path: shown.clone(),
                start: Some(position(&file, error.offset)),
                message: error.message,
            });
        }
        for query in scanned.queries {
            sources.sites.push(Site {
                path: path.clone(),
                file: file.clone(),
                db: query.db,
                sql: query.text,
                literal: query.literal,
            });
        }
        for copy in scanned.copies {
            sources.copies.push(CopySite {
                path: path.clone(),
                file: file.clone(),
                db: copy.db,
                target: copy.target,
                literal: copy.literal,
            });
        }
    }
    sources
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
    for file in &files {
        report.sources.insert(file.path.clone(), file.sql.clone());
    }
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
