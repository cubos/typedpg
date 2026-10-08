use std::path::Path;
use std::process;

use clap::Parser;
use typedpg::migrate::MigrationSource;
use typedpg_cli::{MigrateAction, connect, create_migration, run_migrate_action};
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
        let (up_file, down_file) = create_migration(&migrations_dir, name)?;
        println!("Created {}", up_file.display());
        println!("Created {}", down_file.display());
        return Ok(());
    }

    // Actions below require a database connection
    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL environment variable must be set")?;

    let source = MigrationSource::from_dir(&migrations_dir)?;

    let mut client = connect(&database_url).await?;
    run_migrate_action(action, &mut client, &source, &config.migrations).await
}
