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

mod bindings;
mod scan;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use typedpg_analyzer::PgCatalog;
use typedpg_core::QualifiedName;
use typedpg_core::config::{Config, FINITE_PARAMETER_TYPES};
use typedpg_core::migrations::compare_migration_names;
use typedpg_lint::report::{
    Entry, Failure, Format, Position, Report, position, stderr_colors, stdout_colors,
};
use typedpg_lint::{
    Finding, IgnoreSet, LintOptions, MigrationFile, MigrationLint, lint_migrations, lint_query,
};

/// The directories under the manifest's directory that hold Rust sources.
const SOURCE_DIRS: &[&str] = &["src", "tests", "examples", "benches"];

/// One `sql!` site, with the file text its offsets are in.
struct Site {
    path: PathBuf,
    file: String,
    db: Option<String>,
    sql: String,
    literal: scan::Literal,
    bindings: Vec<scan::Binding>,
}

/// A finding's fix, pending the `lint.ignore` retain and `--fix`.
struct PendingFix {
    /// The file the edits apply to, as scanned.
    path: PathBuf,
    /// The path the report shows.
    shown: String,
    /// The entry the fix belongs to: dropped when the retain drops it.
    entry: Entry,
    /// The scanned file text the edits' offsets are in.
    scanned: String,
    /// File byte ranges and their replacements.
    edits: Vec<(Range<usize>, String)>,
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
///
/// With `ignore_migrations_from`, migration files identical to that git ref
/// have their statement rules skipped; the schema rules still report on the
/// statements that created the schema's objects.
///
/// With `fix`, the findings that have edits are applied to their files, and
/// the report is built again from the fixed text so its positions match.
pub fn run(
    manifest: &Path,
    format: Format,
    ignore_migrations_from: Option<&str>,
    fix: bool,
) -> Result<bool, String> {
    let start = Instant::now();
    let (report, options, pending) = build(manifest, ignore_migrations_from, start);
    if !fix || pending.is_empty() {
        return Ok(finish(&report, &options, format));
    }
    let outcome = apply_fixes(&pending);
    eprintln!(
        "fixed {} {} in {} {}",
        outcome.fixed,
        if outcome.fixed == 1 {
            "finding"
        } else {
            "findings"
        },
        outcome.files,
        if outcome.files == 1 { "file" } else { "files" }
    );
    // The findings' positions were in the text before the fixes; build the
    // report from the files as they are now.
    let (mut report, options, _) = build(manifest, ignore_migrations_from, start);
    report.failures.extend(outcome.failures);
    report.load_failure_sources(&manifest_dir(manifest));
    report.sort();
    Ok(finish(&report, &options, format))
}

/// Scan the crate and lint it, returning the report, the options it was
/// built with, and the fixes its findings could apply.
fn build(
    manifest: &Path,
    ignore_migrations_from: Option<&str>,
    start: Instant,
) -> (Report, LintOptions, Vec<PendingFix>) {
    let base = manifest_dir(manifest);
    let config = match Config::from_cargo_toml(manifest) {
        Ok(config) => config,
        Err(error) => {
            return (
                config_error_report(error, manifest, &base, start),
                LintOptions::default(),
                Vec::new(),
            );
        }
    };
    let resolved = match config.resolve(None) {
        Ok(resolved) => resolved,
        Err(error) => {
            return (
                config_error_report(error, manifest, &base, start),
                LintOptions::default(),
                Vec::new(),
            );
        }
    };
    let options = LintOptions::from_config(&resolved.lint);
    let ignore = match IgnoreSet::new(&resolved.lint.ignore) {
        Ok(ignore) => ignore,
        Err(error) => {
            return (
                config_error_report(error, manifest, &base, start),
                LintOptions::default(),
                Vec::new(),
            );
        }
    };
    let Sources {
        sites,
        copies,
        failures,
    } = scan_sources(&base);
    let mut report = Report {
        queries: sites.len() + copies.len(),
        failures,
        migrations_unchanged_from: ignore_migrations_from.map(str::to_owned),
        ..Report::default()
    };
    // The default database, then every configured one: a named database's
    // migrations are linted even when no query names it.
    let mut databases: Vec<Option<String>> = vec![None];
    databases.extend(config.database_names().into_iter().map(Some));

    let mut catalogs: HashMap<Option<String>, Result<PgCatalog, String>> = HashMap::new();
    let mut migration_paths = HashSet::new();
    for db in &databases {
        let catalog = match config.resolve(db.as_deref()) {
            Ok(_) => catalog_for(
                &config,
                db.as_deref(),
                &base,
                &MigrationSettings {
                    options: &options,
                    ignore: &ignore,
                    ignore_migrations_from,
                },
                &mut migration_paths,
                &mut report,
            ),
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

    let mut pending = Vec::new();
    let mut cache: HashMap<(Option<String>, String), LintResult> = HashMap::new();
    for site in &sites {
        let path = relative(&site.path, &base);
        report.sources.insert(path.clone(), site.file.clone());
        // The binding rules read the Rust source alone: report them even
        // when this database's catalog could not be built.
        for binding in
            bindings::binding_findings(&site.sql, &site.literal, &site.bindings, &options)
        {
            let entry = Entry {
                path: path.clone(),
                start: position(&site.file, binding.finding.span.start),
                end: position(&site.file, binding.finding.span.end),
                finding: binding.finding,
            };
            report.entries.push(entry.clone());
            pending.push(PendingFix {
                path: site.path.clone(),
                shown: path.clone(),
                entry,
                scanned: site.file.clone(),
                edits: binding.edits,
            });
        }
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

    // `lint.ignore` drops findings only: a failure above still fails the run.
    // A matching migration file is linted schema-only instead, so its schema
    // findings stay placed in it.
    report
        .entries
        .retain(|entry| !ignore.is_ignored(&entry.path) || migration_paths.contains(&entry.path));
    // A fix whose finding was dropped is not applied either.
    pending.retain(|fix| report.entries.contains(&fix.entry));
    report.load_failure_sources(&base);
    report.sort();
    report.elapsed = start.elapsed();
    (report, options, pending)
}

/// The report of a configuration that could not be read: one failure.
fn config_error_report(
    error: impl std::fmt::Display,
    manifest: &Path,
    base: &Path,
    start: Instant,
) -> Report {
    let mut report = Report {
        elapsed: start.elapsed(),
        ..Report::default()
    };
    report.failures.push(Failure {
        path: relative(manifest, base),
        start: None,
        message: error.to_string(),
    });
    report
}

/// What applying the pending fixes changed, and the failures that kept a
/// file from being fixed.
struct FixOutcome {
    /// How many findings' edits a written file received.
    fixed: usize,
    /// How many files were written.
    files: usize,
    /// Files that could not be fixed: changed while linting, or unreadable.
    failures: Vec<Failure>,
}

/// Apply the pending fixes file by file.
///
/// A finding's edits are all applied or none: one overlapping an edit
/// accepted before it is skipped. The edits are applied to the text that was
/// scanned — never to a text read again — and a file that changed in between
/// is left untouched rather than risk corrupting it.
fn apply_fixes(pending: &[PendingFix]) -> FixOutcome {
    let mut outcome = FixOutcome {
        fixed: 0,
        files: 0,
        failures: Vec::new(),
    };
    let mut groups: Vec<(&Path, Vec<&PendingFix>)> = Vec::new();
    for fix in pending {
        match groups
            .iter_mut()
            .find(|(path, _)| *path == fix.path.as_path())
        {
            Some((_, fixes)) => fixes.push(fix),
            None => groups.push((fix.path.as_path(), vec![fix])),
        }
    }
    for (path, mut fixes) in groups {
        let scanned = fixes[0].scanned.clone();
        let shown = fixes[0].shown.clone();
        fixes.sort_by_key(|fix| fix.edits.iter().map(|(range, _)| range.start).min());
        let mut accepted: Vec<&PendingFix> = Vec::new();
        let mut accepted_ranges: Vec<Range<usize>> = Vec::new();
        for fix in fixes {
            let overlaps = fix.edits.iter().any(|(edit, _)| {
                accepted_ranges
                    .iter()
                    .any(|range| edit.start < range.end && range.start < edit.end)
            });
            if !overlaps {
                accepted_ranges.extend(fix.edits.iter().map(|(range, _)| range.clone()));
                accepted.push(fix);
            }
        }
        let current = match std::fs::read_to_string(path) {
            Ok(current) => current,
            Err(error) => {
                outcome.failures.push(Failure {
                    path: shown,
                    start: None,
                    message: error.to_string(),
                });
                continue;
            }
        };
        if current != scanned {
            outcome.failures.push(Failure {
                path: shown,
                start: None,
                message: "changed while linting; not fixed".to_owned(),
            });
            continue;
        }
        let mut edits: Vec<&(Range<usize>, String)> =
            accepted.iter().flat_map(|fix| &fix.edits).collect();
        edits.sort_by_key(|(range, _)| range.start);
        let mut fixed = scanned.clone();
        for (range, replacement) in edits.into_iter().rev() {
            fixed.replace_range(range.clone(), replacement);
        }
        if fixed != scanned {
            if let Err(error) = std::fs::write(path, &fixed) {
                outcome.failures.push(Failure {
                    path: shown,
                    start: None,
                    message: error.to_string(),
                });
                continue;
            }
            outcome.fixed += accepted.len();
            outcome.files += 1;
        }
    }
    outcome
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
                bindings: query.bindings,
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

/// How a run lints the migrations: its rule options, its `lint.ignore`
/// patterns and the `--ignore-migrations-from` reference.
struct MigrationSettings<'a> {
    options: &'a LintOptions,
    ignore: &'a IgnoreSet,
    ignore_migrations_from: Option<&'a str>,
}

/// Build the catalog of database `db` from its migrations, reporting the
/// failures that keep it from being built.
fn catalog_for(
    config: &Config,
    db: Option<&str>,
    base: &Path,
    settings: &MigrationSettings<'_>,
    migration_paths: &mut HashSet<String>,
    report: &mut Report,
) -> Result<PgCatalog, String> {
    let MigrationSettings {
        options,
        ignore,
        ignore_migrations_from,
    } = *settings;
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
    for (_, file) in &files {
        report.sources.insert(file.path.clone(), file.sql.clone());
        migration_paths.insert(file.path.clone());
    }
    let mut lint_files: Vec<MigrationFile> = files.iter().map(|(_, file)| file.clone()).collect();
    if let Some(reference) = ignore_migrations_from {
        let candidates: Vec<PathBuf> = files
            .iter()
            .filter(|(_, file)| file.lint == MigrationLint::All)
            .map(|(path, _)| path.clone())
            .collect();
        if !candidates.is_empty() {
            let unchanged = match crate::unchanged_from_ref(reference, &candidates) {
                Ok(unchanged) => unchanged,
                Err(error) => {
                    report.failures.push(Failure {
                        path: relative(&migrations_dir, base),
                        start: None,
                        message: error.clone(),
                    });
                    return Err(error);
                }
            };
            for (path, file) in files.iter().zip(&mut lint_files) {
                if file.lint == MigrationLint::All && unchanged.contains(&path.0) {
                    file.lint = MigrationLint::SchemaOnly;
                    report.migrations_unchanged += 1;
                }
            }
        }
    }
    // An ignored migration file still builds the schema, and its schema
    // findings stay placed in it: only its statement rules are skipped.
    for file in &mut lint_files {
        if file.lint != MigrationLint::None && ignore.is_ignored(&file.path) {
            file.lint = MigrationLint::SchemaOnly;
        }
    }
    let linted = match lint_migrations(&lint_files, resolved.migrations_use_transaction(), options)
    {
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
        let file = &files[migration.file].1;
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

/// The database's migration files, with their paths on disk, in the order the
/// macro applies them: the runner's directory's files are reported, the extra
/// directories' are applied but never reported.
fn collect_migration_files(
    dirs: &[(PathBuf, bool)],
    base: &Path,
) -> Result<Vec<(PathBuf, MigrationFile)>, String> {
    let mut files: Vec<(OsString, PathBuf, String, String, bool)> = Vec::new();
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
            files.push((
                entry.file_name(),
                path.clone(),
                relative(&path, base),
                sql,
                *lint,
            ));
        }
    }
    files.sort_by(|a, b| compare_migration_names(&a.0.to_string_lossy(), &b.0.to_string_lossy()));
    Ok(files
        .into_iter()
        .map(|(_, source, path, sql, lint)| {
            (
                source,
                MigrationFile {
                    path,
                    sql,
                    lint: if lint {
                        MigrationLint::All
                    } else {
                        MigrationLint::None
                    },
                },
            )
        })
        .collect())
}
