use std::collections::{HashMap, HashSet};
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
        } => lint(&config, format, ignore_migrations_from.as_deref()),
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
fn lint(
    config: &Path,
    format: Format,
    ignore_migrations_from: Option<&str>,
) -> Result<bool, String> {
    let start = Instant::now();
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
                report.entries.push(Entry {
                    path: relative(query.file, &root),
                    start: position(query.lines.text(), query.source.offset(finding.span.start)),
                    end: position(query.lines.text(), query.source.offset(finding.span.end)),
                    finding: finding.clone(),
                });
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
    report.load_failure_sources(&root);
    report.sort();
    report.elapsed = start.elapsed();
    let rendered = report.render_with(format, &options, stdout_colors());
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    eprintln!("{}", report.summary_with(&options, stderr_colors()));
    Ok(!report.fails(&options))
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
