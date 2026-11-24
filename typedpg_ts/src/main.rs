use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use typedpg::migrate::MigrationSource;
use typedpg_cli::{MigrateAction, connect, create_migration, run_migrate_action};
use typedpg_lint::report::{
    Entry, Failure, Format, Position, Report, position, stderr_colors, stdout_colors,
};
use typedpg_lint::{CallFinding, IgnoreSet, LintOptions, MigrationLint, Rule};
use typedpg_ts::cache;
use typedpg_ts::config::{CONFIG_FILE, Config};
use typedpg_ts::project::{LintCall, Project};
use typedpg_ts::scan::LineIndex;
use typedpg_ts::watch;

#[derive(Parser)]
#[command(
    name = "typedpg",
    version,
    about = "Typed PostgreSQL queries for TypeScript"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate each database's module from the queries in the sources.
    Gen {
        /// The config file.
        #[arg(long, default_value = CONFIG_FILE)]
        config: PathBuf,
        /// Keep running, regenerating as sources and migrations change.
        #[arg(long)]
        watch: bool,
    },
    /// Fail if a generated module is out of date or a query has an error.
    Check {
        #[arg(long, default_value = CONFIG_FILE)]
        config: PathBuf,
    },
    /// Lint every query: print each finding and fail on the configured
    /// severity.
    Lint {
        #[arg(long, default_value = CONFIG_FILE)]
        config: PathBuf,
        /// How to print the report: `human`, `json` or `sarif`.
        #[arg(long, default_value = "human")]
        format: Format,
        /// The git ref of the code and migrations deployed now (e.g.
        /// `origin/main`): migrations identical to it skip the statement
        /// rules (the schema rules still report on the statements that
        /// created their objects), and `breaks_deployed_queries` checks the
        /// queries of its code against the new migrations.
        #[arg(long, alias = "ignore-migrations-from")]
        previous_deployment: Option<String>,
        /// Apply the findings' SQL fixes to the sources and `.sql` files,
        /// then lint again.
        #[arg(long)]
        fix: bool,
    },
    /// Compile every .sql file into the cache the loaders and bundler
    /// plugins read, so that importing one runs no analysis.
    Build {
        #[arg(long, default_value = CONFIG_FILE)]
        config: PathBuf,
        /// Print the outcome as JSON on stdout (what the loaders read).
        #[arg(long)]
        json: bool,
    },
    /// Serve TypeScript as the content mapper of .sql files, and compile
    /// .sql files for the loaders (JSON-RPC on stdin / stdout).
    Serve {
        /// Exit after this many seconds without a request (default: never).
        #[arg(long)]
        idle_timeout: Option<f64>,
    },
    /// Run database migrations.
    Migrate {
        #[arg(long, default_value = CONFIG_FILE, global = true)]
        config: PathBuf,
        /// The database, when the config has several.
        #[arg(long, global = true)]
        db: Option<String>,
        /// The connection URL; defaults to the DATABASE_URL environment
        /// variable (a `.env` file is read; the environment wins over it).
        #[arg(long, global = true)]
        url: Option<String>,
        #[command(subcommand)]
        action: MigrateAction,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Gen {
            config,
            watch: true,
        } => watch::run(&config).map(|()| true),
        Command::Gen { config, .. } => generate(&config, false),
        Command::Check { config } => generate(&config, true),
        Command::Lint {
            config,
            format,
            previous_deployment,
            fix,
        } => lint(&config, format, previous_deployment.as_deref(), fix),
        Command::Build { config, json } => build(&config, json),
        Command::Serve { idle_timeout } => typedpg_ts::serve::run(idle_timeout).map(|()| true),
        Command::Migrate {
            config,
            db,
            url,
            action,
        } => migrate(&config, db.as_deref(), url, action).map(|()| true),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// One generation (or check); whether it succeeded.
fn generate(config: &Path, check: bool) -> Result<bool, String> {
    let start = Instant::now();
    let mut project = Project::new(Config::load(config)?, check)?;
    let report = project.sync(check);
    if check {
        let root = &project.config.root;
        for d in &report.diagnostics {
            eprintln!("{}", d.render(root));
        }
        for p in &report.changed {
            eprintln!(
                "{}: out of date, run `typedpg gen`",
                p.strip_prefix(root).unwrap_or(p).display()
            );
        }
        for p in &report.removed {
            eprintln!(
                "{}: generated for a .sql file that is gone, run `typedpg gen`",
                p.strip_prefix(root).unwrap_or(p).display()
            );
        }
        return Ok(report.diagnostics.is_empty()
            && report.changed.is_empty()
            && report.removed.is_empty());
    }
    watch::print_report(&project, &report, start, "generated");
    Ok(report.diagnostics.is_empty())
}

/// A query's findings, or its analysis error and offset.
type LintResult = Result<Vec<CallFinding>, (String, Option<usize>)>;

/// `path` as the report shows it: relative to `root`.
fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// Lint every query; whether no analysis failed and no finding reaches
/// `fail_on`.
///
/// With `previous_deployment`, migration files identical to that git ref
/// have their statement rules skipped; the schema rules still report on the
/// statements that created the schema's objects. The queries of the code at
/// that ref are checked against the new migrations
/// (`breaks_deployed_queries`).
///
/// With `fix`, the findings that have edits are applied to their files, and
/// the report is built again from the fixed text so its positions match.
fn lint(
    config: &Path,
    format: Format,
    previous_deployment: Option<&str>,
    fix: bool,
) -> Result<bool, String> {
    let start = Instant::now();
    let (report, options, pending, root) = build_lint(config, previous_deployment, start)?;
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
    let (mut report, options, _, _) = build_lint(config, previous_deployment, start)?;
    report.failures.extend(outcome.failures);
    report.load_failure_sources(&root);
    report.sort();
    Ok(finish(&report, &options, format))
}

/// Lint the project, returning the report, the options it was built with,
/// the fixes its findings could apply, and the project root.
fn build_lint(
    config_path: &Path,
    previous_deployment: Option<&str>,
    start: Instant,
) -> Result<(Report, LintOptions, Vec<PendingFix>, PathBuf), String> {
    // A configuration that can't be read is the run's one failure, in every
    // format: nothing was checked, and the run fails.
    let config_error = |error: String| {
        let shown = config_path.display().to_string().replace('\\', "/");
        let message = error
            .strip_prefix(&format!("{}: ", config_path.display()))
            .map_or_else(|| error.clone(), str::to_owned);
        let report = Report {
            failures: vec![Failure {
                path: shown,
                start: None,
                message,
            }],
            elapsed: start.elapsed(),
            config_error: true,
            ..Report::default()
        };
        (
            report,
            LintOptions::default(),
            Vec::new(),
            PathBuf::from("."),
        )
    };
    let config = match Config::load(config_path) {
        Ok(config) => config,
        Err(error) => return Ok(config_error(error)),
    };
    let options = LintOptions::from_config(&config.lint);
    let ignore = match IgnoreSet::new(&config.lint.ignore) {
        Ok(ignore) => ignore,
        Err(error) => return Ok(config_error(error)),
    };
    let mut project = match Project::for_lint(config) {
        Ok(project) => project,
        Err(error) => return Ok(config_error(error)),
    };
    let root = project.config.root.clone();
    let mut report = Report {
        previous_deployment: previous_deployment.map(str::to_owned),
        ..Report::default()
    };
    // The migration lints are started before the check below and the
    // queries: each builds the schema again on a thread of its own, and the
    // findings are added to the report once they are done.
    let mut report_sources: Vec<(String, String)> = Vec::new();
    let mut migrations_unchanged = 0;
    // `(database, text, calls)` → its findings or analysis error. The same
    // query is often written at several call sites.
    let mut cache: HashMap<(usize, String, Vec<typedpg_lint::Call>, bool), LintResult> =
        HashMap::new();
    let mut migration_paths = HashSet::new();
    // Per database: its migrations as linted, and the first one the
    // previous deployment doesn't have as is.
    let mut new_migrations: Vec<Option<(Vec<typedpg_lint::MigrationFile>, Option<usize>)>> =
        Vec::new();
    let mut migration_lints = Vec::new();
    for db in 0..project.database_count() {
        new_migrations.push(None);
        let files = match project.lint_migration_files(db) {
            Ok(files) => files,
            Err(_) => continue,
        };
        report.migrations += files.len();
        let mut lint_files: Vec<typedpg_lint::MigrationFile> = files
            .iter()
            .map(|file| typedpg_lint::MigrationFile {
                path: relative(&file.path, &root),
                sql: file.sql.clone(),
                lint: file.lint,
            })
            .collect();
        let mut first_new = None;
        if let Some(reference) = previous_deployment {
            let candidates: Vec<PathBuf> = files
                .iter()
                .filter(|file| file.lint == MigrationLint::All)
                .map(|file| file.path.clone())
                .collect();
            if !candidates.is_empty() {
                let unchanged = typedpg_cli::unchanged_from_ref(reference, &candidates)?;
                for (index, (file, lint_file)) in files.iter().zip(&mut lint_files).enumerate() {
                    if lint_file.lint != MigrationLint::All {
                        continue;
                    }
                    if unchanged.contains(&file.path) {
                        lint_file.lint = MigrationLint::SchemaOnly;
                        migrations_unchanged += 1;
                    } else if first_new.is_none() {
                        first_new = Some(index);
                    }
                }
            }
        }
        // An ignored migration file still builds the schema, and its schema
        // findings stay placed in it: only its statement rules are skipped.
        for lint_file in &mut lint_files {
            if lint_file.lint != MigrationLint::None && ignore.is_ignored(&lint_file.path) {
                lint_file.lint = MigrationLint::SchemaOnly;
            }
        }
        for file in &lint_files {
            report_sources.push((file.path.clone(), file.sql.clone()));
            migration_paths.insert(file.path.clone());
        }
        // Linted on a thread of its own, which builds the schema again,
        // while the queries are: its findings are added below.
        let (linted_files, use_transaction, linted_options) = (
            lint_files.clone(),
            project.migrations_use_transaction(db),
            options.clone(),
        );
        let linting = std::thread::Builder::new()
            .stack_size(typedpg_ts::parallel::STACK)
            .spawn(move || {
                typedpg_lint::lint_migrations(&linted_files, use_transaction, &linted_options)
                    .map(|linted| linted.findings)
            })
            .expect("failed to spawn a thread");
        migration_lints.push((files, linting));
        new_migrations[db] = Some((lint_files, first_new));
    }
    // The check `cargo build` would run: every diagnostic it reports is a
    // failure here too (a query or COPY target that does not analyze, a
    // scanner error, a `.sql` layout error, a broken migration). Stale
    // generated modules only need `typedpg gen`.
    let sync = project.sync(true);
    for diagnostic in &sync.diagnostics {
        report.failures.push(Failure {
            path: relative(&diagnostic.file, &root),
            start: diagnostic.pos.map(|pos| Position {
                line: pos.line,
                column: pos.col,
            }),
            message: diagnostic.message.clone(),
        });
    }
    report.migrations_unchanged = migrations_unchanged;
    report.sources.extend(report_sources);
    if let Some(reference) = previous_deployment
        && options.level(Rule::BreaksDeployedQueries).is_some()
    {
        match deployed_project(reference, config_path) {
            Ok(Some((_snapshot, deployed))) => {
                let deployed_root = deployed.config.root.clone();
                let mut queries: Vec<Vec<typedpg_lint::DeployedQuery>> =
                    vec![Vec::new(); deployed.database_count()];
                // The queries, then the `copyIn()` targets: the deployed
                // code sends each target's rows with `COPY … FROM STDIN`.
                let statements = deployed
                    .lint_queries()
                    .into_iter()
                    .map(|query| {
                        (
                            query.db,
                            query.file,
                            query.pos,
                            query.text,
                            typedpg_lint::DeployedKind::Query,
                        )
                    })
                    .chain(deployed.lint_copy_ins().into_iter().map(|copy| {
                        (
                            copy.db,
                            copy.file,
                            copy.pos,
                            copy.target,
                            typedpg_lint::DeployedKind::CopyIn,
                        )
                    }));
                for (db, file, pos, sql, kind) in statements {
                    let location = format!("{}:{}", relative(file, &deployed_root), pos.line);
                    let queries = &mut queries[db];
                    match queries.iter_mut().find(|q| q.sql == sql && q.kind == kind) {
                        Some(q) => q.locations.push(location),
                        None => queries.push(typedpg_lint::DeployedQuery {
                            sql,
                            locations: vec![location],
                            // Every statement of the database runs as each
                            // of its configured roles.
                            roles: deployed.config.databases[db].roles.clone(),
                            kind,
                        }),
                    }
                }
                for (db, migrations) in new_migrations.iter().enumerate() {
                    let Some((files, Some(first_new))) = migrations else {
                        continue;
                    };
                    let name = &project.config.databases[db].name;
                    let Some(old) = deployed
                        .config
                        .databases
                        .iter()
                        .position(|database| database.name == *name)
                    else {
                        continue;
                    };
                    let (Ok(catalog), Ok(old_catalog)) =
                        (project.catalog(db), deployed.catalog(old))
                    else {
                        continue;
                    };
                    let findings = typedpg_lint::breaks_deployed_queries(
                        &typedpg_lint::Deployment {
                            reference,
                            catalog: old_catalog,
                            queries: &queries[old],
                            decoding: typedpg_lint::Decoding::TypeScript {
                                types: deployed.type_overrides(old),
                            },
                        },
                        &typedpg_lint::NewSchema {
                            files,
                            first_new: *first_new,
                            use_transaction: project.migrations_use_transaction(db),
                            catalog,
                            prepare: &|catalog| project.prepare_catalog(db, catalog),
                        },
                        &options,
                    );
                    for migration in findings {
                        let file = &files[migration.file];
                        report.entries.push(Entry {
                            path: file.path.clone(),
                            start: position(&file.sql, migration.finding.span.start),
                            end: position(&file.sql, migration.finding.span.end),
                            finding: migration.finding,
                        });
                    }
                }
            }
            Ok(None) => {}
            Err(message) => report.failures.push(Failure {
                path: relative(config_path, &root),
                start: None,
                message,
            }),
        }
    }
    let queries = project.lint_queries();
    report.queries = queries.len() + project.lint_copy_in_count();
    lint_distinct_queries(&mut cache, &project, &queries, &options);
    let mut pending = Vec::new();
    for query in queries {
        report
            .sources
            .entry(relative(query.file, &root))
            .or_insert_with(|| query.lines.text().to_owned());
        let catalog = match project.catalog(query.db) {
            Ok(catalog) => catalog,
            Err(_) => continue,
        };
        let calls: Vec<typedpg_lint::Call> = query.calls.iter().map(|call| call.call()).collect();
        let roles = &project.config.databases[query.db].roles;
        let result = cache
            .entry((query.db, query.text.clone(), calls.clone(), query.escapes))
            .or_insert_with(|| {
                lint_query(catalog, &query.text, &calls, query.escapes, roles, &options)
            });
        if let Ok(findings) = result {
            for placed in findings {
                let finding = &placed.finding;
                let entry = match placed.call {
                    Some(index) => call_entry(
                        &mut report,
                        &query.calls[index],
                        placed.value,
                        finding,
                        &root,
                    ),
                    None => Entry {
                        path: relative(query.file, &root),
                        start: position(
                            query.lines.text(),
                            query.source.offset(finding.span.start),
                        ),
                        end: position(query.lines.text(), query.source.offset(finding.span.end)),
                        finding: finding.clone(),
                    },
                };
                report.entries.push(entry.clone());
                // A call-site finding is reported at the call, but its fix
                // edits the SQL: write the file the query is in.
                if let Some(edits) = query.source.fix_edits(&finding.fix) {
                    pending.push(PendingFix {
                        path: query.file.to_path_buf(),
                        shown: relative(query.file, &root),
                        entry,
                        scanned: query.lines.text().to_owned(),
                        edits,
                    });
                }
            }
        }
        // An analysis error was already reported by `sync` above.
    }
    // The `copyIn()` targets' `execute` calls: `json_stringify_param` on the
    // rows they pass.
    for copy in project.lint_copy_ins() {
        if copy.calls.iter().all(|call| call.stringified.is_empty()) {
            continue;
        }
        let Ok(catalog) = project.catalog(copy.db) else {
            continue;
        };
        let calls: Vec<typedpg_lint::Call> = copy.calls.iter().map(|call| call.call()).collect();
        // A target that does not analyze was already reported by `sync`.
        let Ok(findings) = typedpg_lint::lint_copy_in_called(
            catalog,
            &copy.target,
            &calls,
            &project.config.databases[copy.db].roles,
            &options,
        ) else {
            continue;
        };
        for placed in findings {
            let Some(index) = placed.call else {
                continue;
            };
            let entry = call_entry(
                &mut report,
                &copy.calls[index],
                placed.value,
                &placed.finding,
                &root,
            );
            report.entries.push(entry);
        }
    }
    for (files, linting) in migration_lints {
        let linted = linting
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        if let Ok(findings) = linted {
            for migration in findings {
                let file = &files[migration.file];
                report.entries.push(Entry {
                    path: relative(&file.path, &root),
                    start: position(&file.sql, migration.finding.span.start),
                    end: position(&file.sql, migration.finding.span.end),
                    finding: migration.finding,
                });
            }
        }
    }
    for db in 0..project.database_count() {
        if let Ok(catalog) = project.catalog(db) {
            report.schemas.push((
                project.config.databases[db].name.clone(),
                catalog.user_object_counts(),
            ));
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
    report.load_failure_sources(&root);
    report.sort();
    report.elapsed = start.elapsed();
    Ok((report, options, pending, root))
}

/// The report entry of a call-site finding: at the call's method
/// identifier, or at the call's value `value` (a `JSON.stringify(…)` it
/// passes) when the finding is about one. Records the calling file's text
/// in the report's sources.
fn call_entry(
    report: &mut Report,
    call: &LintCall<'_>,
    value: Option<usize>,
    finding: &typedpg_lint::Finding,
    root: &Path,
) -> Entry {
    let path = relative(call.file, root);
    report
        .sources
        .entry(path.clone())
        .or_insert_with(|| call.lines.text().to_owned());
    let span = value
        .and_then(|value| call.stringified.get(value))
        .map_or_else(|| call.span.clone(), |value| value.span.clone());
    Entry {
        path,
        start: position(call.lines.text(), span.start),
        end: position(call.lines.text(), span.end),
        finding: finding.clone(),
    }
}

/// Lint every distinct query of `queries` into `cache`, across the
/// machine's cores: the report reads them from it in order.
fn lint_distinct_queries(
    cache: &mut HashMap<(usize, String, Vec<typedpg_lint::Call>, bool), LintResult>,
    project: &Project,
    queries: &[typedpg_ts::project::LintQuery<'_>],
    options: &LintOptions,
) {
    let mut seen = HashSet::new();
    let mut distinct = Vec::new();
    for query in queries {
        let Ok(catalog) = project.catalog(query.db) else {
            continue;
        };
        let calls: Vec<typedpg_lint::Call> = query.calls.iter().map(|call| call.call()).collect();
        let key = (query.db, query.text.clone(), calls, query.escapes);
        if !cache.contains_key(&key) && seen.insert(key.clone()) {
            distinct.push((key, catalog, &project.config.databases[query.db].roles));
        }
    }
    let results =
        typedpg_ts::parallel::map(&distinct, |((_, text, calls, escapes), catalog, roles)| {
            lint_query(catalog, text, calls, *escapes, roles, options)
        });
    cache.extend(distinct.into_iter().map(|(key, ..)| key).zip(results));
}

/// Lint one query as `typedpg check` analyzes it: as each of the
/// database's `roles`, the rules reading the first one's analysis. An
/// `escapes` query's calls can't tell that its value is unused.
fn lint_query(
    catalog: &typedpg_analyzer::PgCatalog,
    text: &str,
    calls: &[typedpg_lint::Call],
    escapes: bool,
    roles: &[String],
    options: &LintOptions,
) -> LintResult {
    let mut options = options.clone();
    if escapes {
        options = options.with_rule_off(Rule::UnusedReturning);
    }
    typedpg_lint::lint_query_called_as(catalog, text, calls, roles, &options)
        .map_err(|error| (error.error.to_string(), error.offset))
}

/// The project as git ref `reference` has it, read from a checkout of the
/// ref exactly as the project itself is read: the previous deployment, whose
/// queries `breaks_deployed_queries` checks. The checkout lives as long as
/// the snapshot. `None` when the ref has no typedpg configuration there.
fn deployed_project(
    reference: &str,
    config: &Path,
) -> Result<Option<(typedpg_cli::Snapshot, Project)>, String> {
    let snapshot = typedpg_cli::Snapshot::new(reference, config)?;
    let deployed = snapshot.path(config)?;
    if !deployed.is_file() {
        return Ok(None);
    }
    let Ok(config) = Config::load(&deployed) else {
        return Ok(None);
    };
    let Ok(project) = Project::for_deployed(config) else {
        return Ok(None);
    };
    Ok(Some((snapshot, project)))
}

fn finish(report: &Report, options: &LintOptions, format: Format) -> bool {
    let rendered = report.render_with(format, options, stdout_colors());
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    eprintln!("{}", report.summary_with(options, stderr_colors()));
    !report.fails(options)
}

/// A finding's fix, pending the `lint.ignore` retain and `--fix`.
struct PendingFix {
    /// The file the edits apply to, as read while linting.
    path: PathBuf,
    /// The path the report shows.
    shown: String,
    /// The entry the fix belongs to: dropped when the retain drops it.
    entry: Entry,
    /// The file's text when it was linted: the edits' offsets are in it.
    scanned: String,
    /// File byte ranges and their replacements.
    edits: Vec<(Range<usize>, String)>,
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

/// Compile each .sql file without a cache entry; whether none has an
/// error.
fn build(config: &Path, json: bool) -> Result<bool, String> {
    let start = Instant::now();
    let config = Config::load(config)?;
    let identity = cache::schema_identity(&config)?;
    let dir = cache::dir(&config);
    let root = config.root.clone();
    let mut project = Project::sql_only(config.clone())?;
    let paths: Vec<PathBuf> = project.sql_paths().map(Path::to_path_buf).collect();
    let (mut compiled, mut cached) = (0, 0);
    let mut diagnostics = Vec::new();
    for path in paths {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let key = cache::key(&config, &identity, &path, &content);
        if cache::exists(&dir, &key) {
            cached += 1;
            continue;
        }
        let shown = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        match project.compile_sql(&path, &content) {
            Ok(c) if c.errors.is_empty() => {
                cache::write(&dir, &key, &c.module)
                    .map_err(|e| format!("failed to write the cache in {}: {e}", dir.display()))?;
                compiled += 1;
            }
            Ok(c) => {
                let lines = LineIndex::new(&content);
                for (offset, _, message) in &c.errors {
                    let pos = lines.position(*offset);
                    diagnostics.push((shown.clone(), Some((pos.line, pos.col)), message.clone()));
                }
            }
            Err(e) => diagnostics.push((shown, None, e)),
        }
    }
    if json {
        let diagnostics: Vec<serde_json::Value> = diagnostics
            .iter()
            .map(|(file, pos, message)| {
                serde_json::json!({
                    "file": file,
                    "line": pos.map(|p| p.0),
                    "col": pos.map(|p| p.1),
                    "message": message,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "compiled": compiled, "cached": cached, "diagnostics": diagnostics })
        );
    } else {
        for (file, pos, message) in &diagnostics {
            match pos {
                Some((line, col)) => eprintln!("{file}:{line}:{col}: error: {message}"),
                None => eprintln!("{file}: error: {message}"),
            }
        }
        eprintln!(
            "[typedpg] built: {compiled} compiled, {cached} cached, {} error{} ({} ms)",
            diagnostics.len(),
            if diagnostics.len() == 1 { "" } else { "s" },
            start.elapsed().as_millis()
        );
    }
    Ok(diagnostics.is_empty())
}

fn migrate(
    config: &Path,
    db: Option<&str>,
    url: Option<String>,
    action: MigrateAction,
) -> Result<(), String> {
    let config = Config::load(config)?;
    let db = config.database(db)?;
    // Shown relative to where the command runs, as `cargo typedpg` does.
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let dir = db
        .migrations_dir
        .strip_prefix(&cwd)
        .unwrap_or(&db.migrations_dir);
    if let MigrateAction::Create { name } = &action {
        let (up, down) = create_migration(dir, name).map_err(|e| e.to_string())?;
        println!("Created {}", up.display());
        println!("Created {}", down.display());
        return Ok(());
    }
    typedpg_cli::load_dotenv();
    let url = url
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .ok_or("pass --url or set the DATABASE_URL environment variable")?;
    let source = MigrationSource::from_dir(&db.migrations_dir).map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let mut client = connect(&url).await.map_err(|e| e.to_string())?;
        run_migrate_action(action, &mut client, &source, &db.runner)
            .await
            .map_err(|e| e.to_string())
    })
}
