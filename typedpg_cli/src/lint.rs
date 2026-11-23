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
    Call as LintCall, CallFinding, CallMethod, Decoding, DeployedKind, DeployedQuery, Deployment,
    Finding, IgnoreSet, Language, LintOptions, MigrationFile, MigrationLint, NewSchema, Rule,
    breaks_deployed_queries, lint_migrations, lint_query_called_as,
};

/// The directories under the manifest's directory that hold Rust sources.
const SOURCE_DIRS: &[&str] = &["src", "tests", "examples", "benches"];

/// One `sql!` site, with the file text its offsets are in.
struct Site {
    path: PathBuf,
    file: String,
    db: Option<String>,
    /// The call site's `role = "name"`.
    role: Option<String>,
    sql: String,
    literal: scan::Literal,
    bindings: Vec<scan::Binding>,
    /// The methods called on the query, as the call-site rules read them.
    calls: Vec<scan::Call>,
    /// A use the scan cannot see as a method call: `unused_returning` stays
    /// quiet for the site.
    escapes: bool,
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
    /// The call site's `role = "name"`.
    role: Option<String>,
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
type LintResult = Result<Vec<CallFinding>, AnalysisError>;

/// A COPY target's analysis outcome.
type CopyResult = Result<(), AnalysisError>;

/// Lint `manifest`'s crate; whether no analysis failed and no finding
/// reaches `fail_on`. The report goes to stdout, the summary to stderr.
///
/// With `previous_deployment`, migration files identical to that git ref
/// have their statement rules skipped; the schema rules still report on the
/// statements that created the schema's objects. The queries of the code at
/// that ref are checked against the new migrations
/// (`breaks_deployed_queries`).
///
/// With `fix`, the findings that have edits are applied to their files, and
/// the report is built again from the fixed text so its positions match.
pub fn run(
    manifest: &Path,
    format: Format,
    previous_deployment: Option<&str>,
    fix: bool,
) -> Result<bool, String> {
    let start = Instant::now();
    let (report, options, pending) = build(manifest, previous_deployment, start);
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
    let (mut report, options, _) = build(manifest, previous_deployment, start);
    report.failures.extend(outcome.failures);
    report.load_failure_sources(&manifest_dir(manifest));
    report.sort();
    Ok(finish(&report, &options, format))
}

/// Scan the crate and lint it, returning the report, the options it was
/// built with, and the fixes its findings could apply.
fn build(
    manifest: &Path,
    previous_deployment: Option<&str>,
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
        previous_deployment: previous_deployment.map(str::to_owned),
        ..Report::default()
    };
    // The default database, then every configured one: a named database's
    // migrations are linted even when no query names it.
    let mut databases: Vec<Option<String>> = vec![None];
    databases.extend(config.database_names().into_iter().map(Some));

    let mut catalogs: HashMap<Option<String>, Result<PgCatalog, String>> = HashMap::new();
    let mut new_migrations: HashMap<Option<String>, NewMigrations> = HashMap::new();
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
                    previous_deployment,
                },
                &mut migration_paths,
                &mut report,
            )
            .map(|(catalog, migrations)| {
                new_migrations.insert(db.clone(), migrations);
                catalog
            }),
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

    if let Some(reference) = previous_deployment
        && options.level(Rule::BreaksDeployedQueries).is_some()
    {
        match deployed(reference, manifest, &options) {
            Ok(deployed) => {
                for db in &databases {
                    let (
                        Some(Ok(catalog)),
                        Some(new),
                        Some(old_catalog),
                        Some(queries),
                        Some(types),
                    ) = (
                        catalogs.get(db),
                        new_migrations.get(db),
                        deployed.catalogs.get(db),
                        deployed.queries.get(db),
                        deployed.types.get(db),
                    )
                    else {
                        continue;
                    };
                    let Some(first_new) = new.first_new else {
                        continue;
                    };
                    let finite = &new.finite;
                    let findings = breaks_deployed_queries(
                        &Deployment {
                            reference,
                            catalog: old_catalog,
                            queries,
                            decoding: Decoding::Rust { types },
                        },
                        &NewSchema {
                            files: &new.files,
                            first_new,
                            use_transaction: new.use_transaction,
                            connection: &new.connection,
                            catalog,
                            prepare: &|catalog| catalog.assume_finite_parameters(finite),
                        },
                        &options,
                    );
                    for migration in findings {
                        let file = &new.files[migration.file];
                        report.entries.push(Entry {
                            path: file.path.clone(),
                            start: position(&file.sql, migration.finding.span.start),
                            end: position(&file.sql, migration.finding.span.end),
                            finding: migration.finding,
                        });
                    }
                }
            }
            Err(message) => report.failures.push(Failure {
                path: relative(manifest, &base),
                start: None,
                message,
            }),
        }
    }

    let mut pending = Vec::new();
    type CacheKey = (Option<String>, Vec<String>, String, Vec<LintCall>, bool);
    let mut cache: HashMap<CacheKey, LintResult> = HashMap::new();
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
        let mut calls = Vec::new();
        let mut call_sites = Vec::new();
        let mut escapes = site.escapes;
        for call in &site.calls {
            match call_method(&call.method) {
                Some(method) => {
                    calls.push(LintCall {
                        method,
                        name: call.method.clone(),
                        language: Language::Rust,
                    });
                    call_sites.push(call);
                }
                // A method the rules do not know still uses the query, so
                // `unused_returning` cannot be sure every use is an
                // `execute()`.
                None => escapes = true,
            }
        }
        let mut site_options = options.clone();
        if escapes {
            site_options = site_options.with_rule_off(Rule::UnusedReturning);
        }
        // Analyzed as the roles it runs as, as the macro analyzes it.
        let roles = roles_of(&config, site.db.as_deref(), site.role.as_deref());
        let result = cache
            .entry((
                site.db.clone(),
                roles.clone(),
                site.sql.clone(),
                calls.clone(),
                escapes,
            ))
            .or_insert_with(|| {
                lint_query_called_as(catalog, &site.sql, &calls, &roles, &site_options)
                    .map_err(|error| (error.error.to_string(), error.offset))
            });
        match result {
            Ok(findings) => {
                for placed in findings {
                    let finding = &placed.finding;
                    let entry = match placed.call {
                        Some(index) => {
                            let call = call_sites[index];
                            Entry {
                                path: path.clone(),
                                start: position(&site.file, call.span.start),
                                end: position(&site.file, call.span.end),
                                finding: finding.clone(),
                            }
                        }
                        None => Entry {
                            path: path.clone(),
                            start: position(
                                &site.file,
                                site.literal.source_offset(finding.span.start),
                            ),
                            end: position(&site.file, site.literal.source_offset(finding.span.end)),
                            finding: finding.clone(),
                        },
                    };
                    report.entries.push(entry.clone());
                    // A call-site finding is reported at the call, but its
                    // fix edits the SQL: write the file the literal is in.
                    if let Some(edits) = sql_fix_edits(finding, &site.literal) {
                        pending.push(PendingFix {
                            path: site.path.clone(),
                            shown: path.clone(),
                            entry,
                            scanned: site.file.clone(),
                            edits,
                        });
                    }
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

    let mut copy_cache: HashMap<(Option<String>, Vec<String>, String), CopyResult> = HashMap::new();
    for copy in &copies {
        let path = relative(&copy.path, &base);
        report.sources.insert(path.clone(), copy.file.clone());
        let Ok(catalog) = catalogs.get(&copy.db).expect("every database was built") else {
            continue;
        };
        let roles = roles_of(&config, copy.db.as_deref(), copy.role.as_deref());
        let result = copy_cache
            .entry((copy.db.clone(), roles.clone(), copy.target.clone()))
            .or_insert_with(|| analyze_copy_in(catalog, &copy.target, &roles));
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

/// The roles a call site's statement runs as, as the macro checks it: the
/// call site's `role = "…"`, or database `db`'s configured `roles` (none:
/// the plain analysis).
fn roles_of(config: &Config, db: Option<&str>, role: Option<&str>) -> Vec<String> {
    match role {
        Some(role) => vec![role.to_owned()],
        None => config
            .resolve(db)
            .map(|resolved| resolved.roles().to_vec())
            .unwrap_or_default(),
    }
}

/// A `copy_in!` target analyzed as the macro analyzes it: as each of
/// `roles`, each resolving the target to the same table and columns (or
/// plainly, without roles).
fn analyze_copy_in(catalog: &PgCatalog, target: &str, roles: &[String]) -> CopyResult {
    let located = |error: typedpg_analyzer::LocatedError| (error.error.to_string(), error.offset);
    if roles.is_empty() {
        return catalog
            .analyze_copy_in_located(target)
            .map(|_| ())
            .map_err(located);
    }
    let mut first: Option<(&str, String)> = None;
    for role in roles {
        let analyzed = catalog
            .analyze_copy_in_as(target, role)
            .map_err(|error| (error.to_string(), None))?;
        match &first {
            None => first = Some((role, analyzed.copy_sql)),
            Some((first_role, first_sql)) if *first_sql != analyzed.copy_sql => {
                return Err((
                    format!(
                        "copy target resolves differently for roles {first_role:?} and {role:?}: \
                         `{first_sql}` for the first and `{}` for the second (the roles' \
                         search_path settings differ)",
                        analyzed.copy_sql
                    ),
                    None,
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
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
        config_error: true,
        ..Report::default()
    };
    report.failures.push(Failure {
        path: relative(manifest, base),
        start: None,
        message: error.to_string(),
    });
    report
}

/// The file edits that apply a query finding's SQL fix, or `None` when the
/// finding has none or its literal cannot hold the replacement text.
///
/// The edits' ranges are in the analyzed SQL text; they are mapped into the
/// file text through `literal`, which also tells a raw string (no escapes to
/// decode) from a normal one. A replacement written into a normal literal
/// escapes `\` and `"`; a raw literal cannot hold a `"` without changing its
/// terminator, so a fix containing one is not applied there.
fn sql_fix_edits(
    finding: &Finding,
    literal: &scan::Literal,
) -> Option<Vec<(Range<usize>, String)>> {
    if finding.fix.is_empty() {
        return None;
    }
    let mut edits = Vec::with_capacity(finding.fix.len());
    for edit in &finding.fix {
        let text = if literal.raw_string {
            if edit.text.contains('"') {
                return None;
            }
            edit.text.clone()
        } else {
            escape_in_normal_literal(&edit.text)
        };
        edits.push((
            literal.source_offset(edit.range.start)..literal.source_offset(edit.range.end),
            text,
        ));
    }
    Some(edits)
}

/// `text` escaped for a normal Rust string literal: `\` and `"` get a
/// backslash; newlines stay as written.
fn escape_in_normal_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out
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

/// The call-site rule's view of a scanned method: every method the `sql!`
/// query has (`typedpg_macros`' codegen) is a call; anything else acts like
/// a query with no call site. `fetch_value()` / `fetch_value_optional()`
/// call `fetch_one()` / `fetch_optional()` and fail the same way on a second
/// row.
fn call_method(name: &str) -> Option<CallMethod> {
    match name {
        "fetch_one" | "fetch_one_as" | "fetch_value" => Some(CallMethod::FetchOne),
        "fetch_optional" | "fetch_optional_as" | "fetch_value_optional" => {
            Some(CallMethod::FetchOptional)
        }
        "fetch_all" | "fetch_all_as" => Some(CallMethod::FetchAll),
        "fetch_stream" | "fetch_stream_as" => Some(CallMethod::FetchStream),
        "execute" => Some(CallMethod::Execute),
        _ => None,
    }
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
                role: query.role,
                sql: query.text,
                literal: query.literal,
                bindings: query.bindings,
                calls: query.calls,
                escapes: query.escapes,
            });
        }
        for copy in scanned.copies {
            sources.copies.push(CopySite {
                path: path.clone(),
                file: file.clone(),
                db: copy.db,
                role: copy.role,
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

/// The code deployed at `--previous-deployment`: per database, its
/// queries and the schema its migrations build.
#[derive(Default)]
struct Deployed {
    catalogs: HashMap<Option<String>, PgCatalog>,
    queries: HashMap<Option<String>, Vec<DeployedQuery>>,
    /// Its `[types]` map, per database: what its queries decode into.
    types: HashMap<Option<String>, HashMap<QualifiedName, String>>,
}

/// The queries and schemas of the crate as git ref `reference` has it,
/// read from a checkout of the ref exactly as the crate itself is read.
///
/// A ref where the crate has no typedpg configuration, or a database whose
/// migrations don't build there, has nothing deployed to check.
fn deployed(reference: &str, manifest: &Path, options: &LintOptions) -> Result<Deployed, String> {
    let snapshot = crate::Snapshot::new(reference, manifest)?;
    let old_manifest = snapshot.path(manifest)?;
    let mut deployed = Deployed::default();
    if !old_manifest.is_file() {
        return Ok(deployed);
    }
    let Ok(config) = Config::from_cargo_toml(&old_manifest) else {
        return Ok(deployed);
    };
    let base = manifest_dir(&old_manifest);
    let ignore = IgnoreSet::new(&[])?;
    let mut databases: Vec<Option<String>> = vec![None];
    databases.extend(config.database_names().into_iter().map(Some));
    for db in databases {
        // Its own findings and failures are not this run's.
        let mut scratch = Report::default();
        let built = catalog_for(
            &config,
            db.as_deref(),
            &base,
            &MigrationSettings {
                options,
                ignore: &ignore,
                previous_deployment: None,
            },
            &mut HashSet::new(),
            &mut scratch,
        );
        if let Ok((catalog, _)) = built {
            let types = config
                .resolve(db.as_deref())
                .map(|resolved| resolved.types)
                .unwrap_or_default();
            deployed.types.insert(db.clone(), types);
            deployed.catalogs.insert(db, catalog);
        }
    }
    let sources = scan_sources(&base);
    let statements = sources
        .sites
        .into_iter()
        .map(|site| {
            (
                site.path,
                site.file,
                site.db,
                site.role,
                site.sql,
                site.literal,
                DeployedKind::Query,
            )
        })
        .chain(sources.copies.into_iter().map(|copy| {
            (
                copy.path,
                copy.file,
                copy.db,
                copy.role,
                copy.target,
                copy.literal,
                DeployedKind::CopyIn,
            )
        }));
    for (path, file, db, role, sql, literal, kind) in statements {
        let location = format!(
            "{}:{}",
            relative(&path, &base),
            position(&file, literal.source_offset(0)).line
        );
        // The roles it runs as, as the macro checks it.
        let roles = roles_of(&config, db.as_deref(), role.as_deref());
        let queries = deployed.queries.entry(db).or_default();
        match queries
            .iter_mut()
            .find(|query| query.sql == sql && query.roles == roles && query.kind == kind)
        {
            Some(query) => query.locations.push(location),
            None => queries.push(DeployedQuery {
                sql,
                locations: vec![location],
                roles,
                kind,
            }),
        }
    }
    Ok(deployed)
}

/// A database's migrations as `breaks_deployed_queries` replays them.
struct NewMigrations {
    /// Every file, as linted.
    files: Vec<MigrationFile>,
    /// The first reported file `--previous-deployment` doesn't have as is.
    first_new: Option<usize>,
    use_transaction: bool,
    /// What the configuration names of the migrations' session.
    connection: typedpg_core::config::MigrationsConnection,
    /// The parameter types the catalog assumes finite.
    finite: Vec<QualifiedName>,
}

/// How a run lints the migrations: its rule options, its `lint.ignore`
/// patterns and the `--previous-deployment` reference.
struct MigrationSettings<'a> {
    options: &'a LintOptions,
    ignore: &'a IgnoreSet,
    previous_deployment: Option<&'a str>,
}

/// Build the catalog of database `db` from its migrations, reporting the
/// failures that keep it from being built; with the migrations as linted.
fn catalog_for(
    config: &Config,
    db: Option<&str>,
    base: &Path,
    settings: &MigrationSettings<'_>,
    migration_paths: &mut HashSet<String>,
    report: &mut Report,
) -> Result<(PgCatalog, NewMigrations), String> {
    let MigrationSettings {
        options,
        ignore,
        previous_deployment,
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
    let mut first_new = None;
    if let Some(reference) = previous_deployment {
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
            for (index, (path, file)) in files.iter().zip(&mut lint_files).enumerate() {
                if file.lint != MigrationLint::All {
                    continue;
                }
                if unchanged.contains(&path.0) {
                    file.lint = MigrationLint::SchemaOnly;
                    report.migrations_unchanged += 1;
                } else if first_new.is_none() {
                    first_new = Some(index);
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
    let linted = match lint_migrations(
        &lint_files,
        resolved.migrations_use_transaction(),
        &resolved.migrations_connection(),
        options,
    ) {
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
    let migrations = NewMigrations {
        files: lint_files,
        first_new,
        use_transaction: resolved.migrations_use_transaction(),
        connection: resolved.migrations_connection(),
        finite,
    };
    Ok((catalog, migrations))
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

#[cfg(test)]
mod tests {
    use super::*;
    use typedpg_lint::{Rule, Severity, SqlEdit};

    fn finding(fix: Vec<SqlEdit>) -> Finding {
        Finding {
            rule: Rule::RedundantLimit,
            severity: Severity::Info,
            span: 0..0,
            message: "redundant LIMIT".to_owned(),
            help: None,
            fix,
        }
    }

    /// Apply `edits` to `source`.
    fn apply(source: &str, edits: &[(Range<usize>, String)]) -> String {
        let mut edits = edits.to_vec();
        edits.sort_by_key(|(range, _)| range.start);
        let mut text = source.to_owned();
        for (range, replacement) in edits.into_iter().rev() {
            text.replace_range(range, &replacement);
        }
        text
    }

    #[test]
    fn a_sql_fix_maps_through_an_escape_before_the_range() {
        let source = r#"fn main() { sql!(pool, "SELECT 'a\tb', 1 FROM t"); }"#;
        let scanned = scan::scan(source).unwrap();
        let query = &scanned.queries[0];
        let start = query.text.find(", 1").unwrap();
        let finding = finding(vec![SqlEdit {
            range: start..start + 3,
            text: String::new(),
        }]);
        let edits = sql_fix_edits(&finding, &query.literal).unwrap();
        assert_eq!(
            apply(source, &edits),
            r#"fn main() { sql!(pool, "SELECT 'a\tb' FROM t"); }"#
        );
    }

    #[test]
    fn a_sql_fix_escapes_a_quote_inserted_into_a_normal_literal() {
        let source = r#"fn main() { sql!(pool, "SELECT 2"); }"#;
        let scanned = scan::scan(source).unwrap();
        let query = &scanned.queries[0];
        let at = query.text.find('2').unwrap();
        let finding = finding(vec![SqlEdit {
            range: at..at,
            text: "\"x\"".to_owned(),
        }]);
        let edits = sql_fix_edits(&finding, &query.literal).unwrap();
        assert_eq!(
            apply(source, &edits),
            r#"fn main() { sql!(pool, "SELECT \"x\"2"); }"#
        );
    }

    #[test]
    fn a_sql_fix_with_a_quote_is_skipped_in_a_raw_literal() {
        let source = r####"fn main() { sql!(pool, r#"SELECT 2"#); }"####;
        let scanned = scan::scan(source).unwrap();
        let query = &scanned.queries[0];
        let at = query.text.find('2').unwrap();
        let finding = finding(vec![SqlEdit {
            range: at..at,
            text: "\"x\"".to_owned(),
        }]);
        assert!(sql_fix_edits(&finding, &query.literal).is_none());
    }
}
