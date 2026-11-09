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
use typedpg_lint::{Finding, IgnoreSet, LintOptions, MigrationLint};
use typedpg_ts::cache;
use typedpg_ts::config::{CONFIG_FILE, Config};
use typedpg_ts::project::Project;
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
        /// A git ref (e.g. `origin/main`): migrations identical to it skip
        /// the statement rules; the schema rules still report on the
        /// statements that created their objects.
        #[arg(long)]
        ignore_migrations_from: Option<String>,
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
            ignore_migrations_from,
            fix,
        } => lint(&config, format, ignore_migrations_from.as_deref(), fix),
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
type LintResult = Result<Vec<Finding>, (String, Option<usize>)>;

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
/// With `ignore_migrations_from`, migration files identical to that git ref
/// have their statement rules skipped; the schema rules still report on the
/// statements that created the schema's objects.
///
/// With `fix`, the findings that have edits are applied to their files, and
/// the report is built again from the fixed text so its positions match.
fn lint(
    config: &Path,
    format: Format,
    ignore_migrations_from: Option<&str>,
    fix: bool,
) -> Result<bool, String> {
    let start = Instant::now();
    let (report, options, pending, root) = build_lint(config, ignore_migrations_from, start)?;
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
    let (mut report, options, _, _) = build_lint(config, ignore_migrations_from, start)?;
    report.failures.extend(outcome.failures);
    report.load_failure_sources(&root);
    report.sort();
    Ok(finish(&report, &options, format))
}

/// Lint the project, returning the report, the options it was built with,
/// the fixes its findings could apply, and the project root.
fn build_lint(
    config: &Path,
    ignore_migrations_from: Option<&str>,
    start: Instant,
) -> Result<(Report, LintOptions, Vec<PendingFix>, PathBuf), String> {
    let config = Config::load(config)?;
    let options = LintOptions::from_config(&config.lint);
    let ignore = IgnoreSet::new(&config.lint.ignore)?;
    let mut project = Project::new(config, true)?;
    let root = project.config.root.clone();
    // The check `cargo build` would run: every diagnostic it reports is a
    // failure here too (a query or COPY target that does not analyze, a
    // scanner error, a `.sql` layout error, a broken migration). Stale
    // generated modules only need `typedpg gen`.
    let sync = project.sync(true);
    let mut report = Report {
        migrations_unchanged_from: ignore_migrations_from.map(str::to_owned),
        ..Report::default()
    };
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
    // `(database, text)` → its findings or analysis error. The same query is
    // often written at several call sites.
    let mut cache: HashMap<(usize, String), LintResult> = HashMap::new();
    let mut migration_paths = HashSet::new();
    for db in 0..project.database_count() {
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
        if let Some(reference) = ignore_migrations_from {
            let candidates: Vec<PathBuf> = files
                .iter()
                .filter(|file| file.lint == MigrationLint::All)
                .map(|file| file.path.clone())
                .collect();
            if !candidates.is_empty() {
                let unchanged = typedpg_cli::unchanged_from_ref(reference, &candidates)?;
                for (file, lint_file) in files.iter().zip(&mut lint_files) {
                    if lint_file.lint == MigrationLint::All && unchanged.contains(&file.path) {
                        lint_file.lint = MigrationLint::SchemaOnly;
                        report.migrations_unchanged += 1;
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
            report.sources.insert(file.path.clone(), file.sql.clone());
            migration_paths.insert(file.path.clone());
        }
        if let Ok(linted) = typedpg_lint::lint_migrations(
            &lint_files,
            project.migrations_use_transaction(db),
            &options,
        ) {
            for migration in linted.findings {
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
    let queries = project.lint_queries();
    report.queries = queries.len() + project.lint_copy_in_count();
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
        let result = cache
            .entry((query.db, query.text.clone()))
            .or_insert_with(|| {
                typedpg_lint::lint_query(catalog, &query.text, &options)
                    .map_err(|error| (error.error.to_string(), error.offset))
            });
        if let Ok(findings) = result {
            for finding in findings {
                let entry = Entry {
                    path: relative(query.file, &root),
                    start: position(query.lines.text(), query.source.offset(finding.span.start)),
                    end: position(query.lines.text(), query.source.offset(finding.span.end)),
                    finding: finding.clone(),
                };
                report.entries.push(entry.clone());
                if let Some(edits) = query.source.fix_edits(&finding.fix) {
                    pending.push(PendingFix {
                        path: query.file.to_path_buf(),
                        shown: entry.path.clone(),
                        entry,
                        scanned: query.lines.text().to_owned(),
                        edits,
                    });
                }
            }
        }
        // An analysis error was already reported by `sync` above.
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
