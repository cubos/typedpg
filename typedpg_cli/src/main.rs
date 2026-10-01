use std::fs;
use std::path::Path;
use std::process;

use chrono::Utc;
use clap::Parser;
use typedpg::migrate::{self, MigrationSource};
use typedpg_core::config::Config;

/// Entry point for `cargo typedpg`.
///
/// When invoked as `cargo typedpg migrate run`, Cargo calls `cargo-typedpg typedpg migrate run`.
/// The outer `Typedpg` subcommand absorbs that injected `typedpg` token.
#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo", about = "typedpg database tools")]
struct Cli {
    #[command(subcommand)]
    command: CargoSubcommand,
}

#[derive(clap::Subcommand)]
enum CargoSubcommand {
    /// typedpg database tools
    Typedpg {
        #[command(subcommand)]
        command: Commands,
    },
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Run database migrations
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
}

#[derive(clap::Subcommand)]
enum MigrateAction {
    /// Apply all pending migrations
    Up,
    /// Show migration status
    Status,
    /// Revert a migration (defaults to the last applied)
    Down {
        /// Name of migration to revert (defaults to last applied)
        name: Option<String>,
        /// Force revert even without a .down.sql file
        #[arg(long)]
        force: bool,
    },
    /// Create a new empty migration file
    Create {
        /// Migration name (e.g., "add_users_table")
        name: String,
    },
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv_override();

    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let CargoSubcommand::Typedpg { command } = cli.command;
    match command {
        Commands::Migrate { action } => handle_migrate(action).await,
    }
}

async fn handle_migrate(action: MigrateAction) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_cargo_toml(Path::new("./Cargo.toml"))?;
    // Relative to the current directory: an empty base keeps the configured
    // path as written ("./db", not "././db") in what the CLI prints.
    let migrations_dir = config.migrations_dir(Path::new(""));

    // Handle actions that don't require a database connection
    if let MigrateAction::Create { name } = &action {
        let timestamp = Utc::now().format("%Y%m%d%H%M%S");
        fs::create_dir_all(&migrations_dir)?;

        let up_file = migrations_dir.join(format!("{timestamp}_{name}.sql"));
        let down_file = migrations_dir.join(format!("{timestamp}_{name}.down.sql"));

        fs::write(&up_file, "")?;
        fs::write(&down_file, "")?;

        println!("Created {}", up_file.display());
        println!("Created {}", down_file.display());
        return Ok(());
    }

    // Actions below require a database connection
    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL environment variable must be set")?;

    let source = MigrationSource::from_dir(&migrations_dir)?;

    let mut client = connect(&database_url).await?;
    run_migrate_action(action, &mut client, &source, &config).await
}

/// Connect honoring the URL's `sslmode` as libpq does: `disable` is plain,
/// `require` is TLS or nothing, and `prefer` (the default) tries TLS and
/// falls back to a plain connection when that attempt fails — for instance
/// on a server with a self-signed certificate, which the TLS connector
/// (verifying against the system roots) rejects.
async fn connect(database_url: &str) -> Result<tokio_postgres::Client, Box<dyn std::error::Error>> {
    use tokio_postgres::config::SslMode;

    let config: tokio_postgres::Config = database_url
        .parse()
        .map_err(|e| format!("invalid DATABASE_URL: {e}"))?;
    match config.get_ssl_mode() {
        SslMode::Disable => connect_plain(&config).await,
        SslMode::Require => connect_tls(&config).await,
        _ => match connect_tls(&config).await {
            Ok(client) => Ok(client),
            Err(tls_err) => {
                let mut plain = config.clone();
                plain.ssl_mode(SslMode::Disable);
                connect_plain(&plain)
                    .await
                    .map_err(|e| format!("{e} (after a TLS attempt failed: {tls_err})").into())
            }
        },
    }
}

async fn connect_tls(
    config: &tokio_postgres::Config,
) -> Result<tokio_postgres::Client, Box<dyn std::error::Error>> {
    let tls_connector = native_tls::TlsConnector::builder().build()?;
    let tls = postgres_native_tls::MakeTlsConnector::new(tls_connector);
    let (client, connection) = config.connect(tls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Database connection error: {e}");
        }
    });
    Ok(client)
}

async fn connect_plain(
    config: &tokio_postgres::Config,
) -> Result<tokio_postgres::Client, Box<dyn std::error::Error>> {
    let (client, connection) = config.connect(tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Database connection error: {e}");
        }
    });
    Ok(client)
}

async fn run_migrate_action(
    action: MigrateAction,
    client: &mut tokio_postgres::Client,
    source: &MigrationSource,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        MigrateAction::Up => {
            let applied = migrate::run(client, source, &config.migrations).await?;
            if applied.is_empty() {
                println!("No pending migrations");
            } else {
                for name in &applied {
                    println!("Applying {name}... done");
                }
                println!("Applied {} migration(s)", applied.len());
            }
        }
        MigrateAction::Status => {
            let statuses = migrate::status(client, source, &config.migrations).await?;
            if statuses.is_empty() {
                println!("No migrations found");
            } else {
                for s in &statuses {
                    if s.applied {
                        let ts = s
                            .applied_at
                            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                            .unwrap_or_default();
                        let drift = if s.drifted { " [MODIFIED]" } else { "" };
                        println!("  \u{2713} {:<30} (applied {ts}){drift}", s.name);
                    } else {
                        println!("  \u{00b7} {:<30} (pending)", s.name);
                    }
                }
            }
        }
        MigrateAction::Down { name, force } => {
            let revert_name = match name {
                Some(n) => n,
                None => {
                    // Find the last applied migration
                    let statuses = migrate::status(client, source, &config.migrations).await?;
                    statuses
                        .iter()
                        .rev()
                        .find(|s| s.applied)
                        .map(|s| s.name.clone())
                        .ok_or("No applied migrations to revert")?
                }
            };
            migrate::revert(client, source, &revert_name, force, &config.migrations).await?;
            println!("Reverting {revert_name}... done");
        }
        MigrateAction::Create { .. } => unreachable!(),
    }

    Ok(())
}
