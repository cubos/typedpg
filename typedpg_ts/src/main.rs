use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use typedpg::migrate::MigrationSource;
use typedpg_cli::{MigrateAction, connect, create_migration, run_migrate_action};
use typedpg_ts::config::{CONFIG_FILE, Config};
use typedpg_ts::project::Project;
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
    /// Run database migrations.
    Migrate {
        #[arg(long, default_value = CONFIG_FILE, global = true)]
        config: PathBuf,
        /// The database, when the config has several.
        #[arg(long, global = true)]
        db: Option<String>,
        /// The connection URL; defaults to the DATABASE_URL environment
        /// variable (a `.env` file is read).
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
        return Ok(report.diagnostics.is_empty() && report.changed.is_empty());
    }
    watch::print_report(&project, &report, start, "generated");
    Ok(report.diagnostics.is_empty())
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
    let _ = dotenvy::dotenv_override();
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
