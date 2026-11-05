//! The migration commands behind `cargo typedpg migrate`, shared with the
//! TypeScript CLI (`typedpg migrate`, in `typedpg_ts`), which configures
//! them from `typedpg.config.json` instead of `Cargo.toml`, and
//! `cargo typedpg lint` ([`lint`]).

pub mod lint;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::Utc;
use typedpg::migrate::{self, MigrationSource, MigrationsConfig};

#[derive(clap::Subcommand)]
pub enum MigrateAction {
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
    /// Record an applied migration's edited file as applied, without running it
    Accept {
        /// Name of the applied migration to accept
        name: String,
    },
    /// Create a new empty migration file
    Create {
        /// Migration name (e.g., "add_users_table")
        name: String,
    },
}

/// Write an empty `<timestamp>_<name>.sql` and its `.down.sql` in `dir`;
/// returns the two paths.
///
/// Neither file is overwritten: a migration written by hand, or one created
/// a moment ago, is never silently emptied.
pub fn create_migration(dir: &Path, name: &str) -> std::io::Result<(PathBuf, PathBuf)> {
    let timestamp = Utc::now().format("%Y%m%d%H%M%S");
    fs::create_dir_all(dir)?;
    let up_file = dir.join(format!("{timestamp}_{name}.sql"));
    let down_file = dir.join(format!("{timestamp}_{name}.down.sql"));
    write_new(&up_file)?;
    if let Err(e) = write_new(&down_file) {
        // Nothing is left behind: a migration whose down file could not be
        // written is a half-created one, and the runner would apply it.
        let _ = fs::remove_file(&up_file);
        return Err(e);
    }
    Ok((up_file, down_file))
}

/// Write an empty file, failing when it already exists.
fn write_new(path: &Path) -> std::io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "refusing to overwrite {}: a file with that name already exists (the \
                         migration's name carries a second-resolution timestamp — retry in a \
                         moment, or use a different name)",
                        path.display()
                    ),
                )
            } else {
                e
            }
        })
}

/// The files among `files` whose content is identical to their version at
/// git ref `reference` (`--ignore-migrations-from`).
///
/// `reference` is resolved to a commit; a file absent from it, or different
/// from it, is not returned. Files are compared by blob id, so the same
/// filters a commit applies are applied to the working copies too. Not being
/// in a git repository, or an unknown ref, is an error.
pub fn unchanged_from_ref(reference: &str, files: &[PathBuf]) -> Result<HashSet<PathBuf>, String> {
    let base = files
        .first()
        .and_then(|file| file.parent())
        .unwrap_or(Path::new("."));
    let top = git_text(base, &["rev-parse", "--show-toplevel"]).map_err(|error| {
        format!(
            "{} is not in a git repository (needed by --ignore-migrations-from): {error}",
            base.display()
        )
    })?;
    let top = PathBuf::from(top.trim_end());
    let top = fs::canonicalize(&top).map_err(|error| {
        format!(
            "failed to resolve the git work tree {}: {error}",
            top.display()
        )
    })?;
    let commit = git_text(
        &top,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
    .map_err(|_| format!("unknown git ref `{reference}`"))?;

    // Each file's path relative to the work tree root, as git writes it (with
    // forward slashes), mapped back to the path the caller passed.
    let mut relative: HashMap<String, PathBuf> = HashMap::new();
    for file in files {
        let absolute = fs::canonicalize(file)
            .map_err(|error| format!("failed to read {}: {error}", file.display()))?;
        let path = absolute.strip_prefix(&top).map_err(|_| {
            format!(
                "{} is outside the git work tree {}",
                file.display(),
                top.display()
            )
        })?;
        relative.insert(
            path.components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            file.clone(),
        );
    }
    if relative.is_empty() {
        return Ok(HashSet::new());
    }

    // REF's blob for each path, from `ls-tree -z`; a path missing from REF
    // simply has none.
    let mut args: Vec<OsString> = vec![
        "ls-tree".into(),
        "-r".into(),
        "-z".into(),
        "--full-tree".into(),
        commit.trim().into(),
        "--".into(),
    ];
    args.extend(relative.keys().map(OsString::from));
    let tree = git_output(&top, &args)?;
    let mut reference_blobs: HashMap<&str, &str> = HashMap::new();
    for entry in tree.split(|byte| *byte == 0) {
        // `<[u8]>::split_once` is unstable: split at the tab by hand.
        let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let (meta, path) = (&entry[..tab], &entry[tab + 1..]);
        let mut fields = meta.split(|byte| *byte == b' ');
        let (Some(_mode), Some(_kind), Some(oid)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(path) = std::str::from_utf8(path) else {
            continue;
        };
        if relative.contains_key(path) {
            reference_blobs.insert(path, std::str::from_utf8(oid).unwrap_or(""));
        }
    }
    if reference_blobs.is_empty() {
        return Ok(HashSet::new());
    }

    // Hash the working files with the same filters a commit would apply.
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin-paths"])
        .current_dir(&top)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run git: {error}"))?;
    let mut stdin = child.stdin.take().expect("git's stdin is piped");
    let mut input = String::new();
    for path in reference_blobs.keys() {
        input.push_str(path);
        input.push('\n');
    }
    stdin
        .write_all(input.as_bytes())
        .map_err(|error| format!("failed to run git: {error}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed to run git: {error}"))?;
    if !output.status.success() {
        return Err(git_error("hash-object --stdin-paths", &output.stderr));
    }
    let working = String::from_utf8_lossy(&output.stdout);
    let mut unchanged = HashSet::new();
    for ((path, reference_blob), working_blob) in reference_blobs.iter().zip(working.lines()) {
        if *reference_blob == working_blob {
            unchanged.insert(relative[*path].clone());
        }
    }
    Ok(unchanged)
}

/// Run git in `dir` and return its stdout, or its stderr as the error.
fn git_output(dir: &Path, args: &[OsString]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|error| format!("failed to run git: {error}"))?;
    if !output.status.success() {
        let command = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        return Err(git_error(&command, &output.stderr));
    }
    Ok(output.stdout)
}

/// Run git in `dir` and return its stdout as text.
fn git_text(dir: &Path, args: &[&str]) -> Result<String, String> {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    String::from_utf8(git_output(dir, &args)?)
        .map_err(|error| format!("git printed non-UTF-8 output: {error}"))
}

fn git_error(command: &str, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("git {command} failed")
    } else {
        format!("git {command}: {stderr}")
    }
}

/// Connect honoring the URL's `sslmode` as libpq does: `disable` is plain,
/// `require` is TLS or nothing, and `prefer` (the default) tries TLS and
/// falls back to a plain connection when that attempt fails — for instance
/// on a server that does not offer TLS, or when the TLS handshake itself
/// fails.
///
/// libpq does not verify the certificate for `require` and `prefer`: it only
/// checks that the server presents one. Verifying would make a server with
/// a self-signed certificate unreachable — and `prefer` would then fall
/// back to a plain connection, sending the password and the migration SQL
/// in the clear where libpq keeps the connection encrypted.
///
/// The modes that do verify (`verify-ca`, `verify-full`) are not supported:
/// the connection library behind this CLI has no way to ask for them.
pub async fn connect(
    database_url: &str,
) -> Result<tokio_postgres::Client, Box<dyn std::error::Error>> {
    use tokio_postgres::config::SslMode;

    check_sslmode(database_url)?;
    let config: tokio_postgres::Config = database_url
        .parse()
        .map_err(|e| format!("invalid DATABASE_URL: {e}"))?;
    match config.get_ssl_mode() {
        SslMode::Disable => connect_plain(&config).await,
        SslMode::Require => connect_tls(&config).await,
        _ => match connect_tls(&config).await {
            Ok(client) => Ok(client),
            Err(tls_err) => {
                eprintln!(
                    "typedpg: the TLS handshake failed ({}); falling back to an unencrypted \
                     connection — the password and the migration SQL will travel in clear",
                    tls_err
                );
                let mut plain = config.clone();
                plain.ssl_mode(SslMode::Disable);
                connect_plain(&plain)
                    .await
                    .map_err(|e| format!("{e} (after a TLS attempt failed: {tls_err})").into())
            }
        },
    }
}

/// The `sslmode` the URL asks for, when it is one this CLI cannot honor.
///
/// The connection library takes `disable`, `prefer` and `require`; the modes
/// that verify the server's certificate (`verify-ca`, `verify-full`) are
/// rejected by it with a message that says nothing about which parameter was
/// wrong. The CLI says it instead — and says what it can do.
fn check_sslmode(database_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let Some(value) = parameter(database_url, "sslmode") else {
        return Ok(());
    };
    match value.as_str() {
        "disable" | "prefer" | "require" | "allow" => Ok(()),
        other => Err(format!(
            "unsupported sslmode `{other}`: this CLI connects with `disable`, `prefer` or \
             `require`, and never verifies the server's certificate"
        )
        .into()),
    }
}

/// The value of the `name` parameter in a libpq connection string or URL.
fn parameter(url: &str, name: &str) -> Option<String> {
    let value = if let Some(query) = url.split_once('?').map(|(_, q)| q) {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
    } else {
        url.split_whitespace()
            .find_map(|word| word.strip_prefix(&format!("{name}=")))
    }?;
    Some(value.trim_matches('\'').to_owned())
}

async fn connect_tls(
    config: &tokio_postgres::Config,
) -> Result<tokio_postgres::Client, Box<dyn std::error::Error>> {
    // libpq's `require` and `prefer`: the connection is encrypted, but the
    // certificate is not verified.
    let mut builder = native_tls::TlsConnector::builder();
    builder.danger_accept_invalid_certs(true);
    builder.danger_accept_invalid_hostnames(true);
    let tls_connector = builder.build()?;
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

/// Run `action` (any but `Create`, which needs no connection) with the
/// runner settings `config`.
pub async fn run_migrate_action(
    action: MigrateAction,
    client: &mut tokio_postgres::Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        MigrateAction::Up => {
            let applied = migrate::run(client, source, config).await?;
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
            let statuses = migrate::status(client, source, config).await?;
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
                    let statuses = migrate::status(client, source, config).await?;
                    statuses
                        .iter()
                        .rev()
                        .find(|s| s.applied)
                        .map(|s| s.name.clone())
                        .ok_or("No applied migrations to revert")?
                }
            };
            migrate::revert(client, source, &revert_name, force, config).await?;
            println!("Reverting {revert_name}... done");
        }
        MigrateAction::Accept { name } => {
            migrate::accept(client, source, config, &name).await?;
            println!("Accepted {name}: its current file is recorded as applied (nothing ran)");
        }
        MigrateAction::Create { .. } => unreachable!(),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap_or_else(|error| panic!("git {}: {error}", args.join(" ")));
        assert!(status.success(), "git {} failed", args.join(" "));
    }

    #[test]
    fn unchanged_from_ref_compares_the_working_files_with_the_ref() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        let first = root.join("0001_first.sql");
        let second = root.join("0002_second.sql");
        fs::write(&first, "CREATE TABLE a (id integer);\n").unwrap();
        fs::write(&second, "CREATE TABLE b (id integer);\n").unwrap();
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "add",
                ".",
            ],
        );
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                "two migrations",
            ],
        );

        // One identical, one modified, one added.
        fs::write(&second, "CREATE TABLE b (id bigint);\n").unwrap();
        let third = root.join("0003_third.sql");
        fs::write(&third, "CREATE TABLE c (id integer);\n").unwrap();
        let unchanged =
            unchanged_from_ref("HEAD", &[first.clone(), second.clone(), third.clone()]).unwrap();
        assert_eq!(unchanged, HashSet::from([first]));
    }

    #[test]
    fn unchanged_from_ref_rejects_an_unknown_ref() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        let file = root.join("0001_first.sql");
        fs::write(&file, "CREATE TABLE a (id integer);\n").unwrap();
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "add",
                ".",
            ],
        );
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                "one migration",
            ],
        );
        let error = unchanged_from_ref("origin/nope", &[file]).unwrap_err();
        assert!(error.contains("origin/nope"), "{error}");
    }

    #[test]
    fn unchanged_from_ref_needs_a_git_repository() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("0001_first.sql");
        fs::write(&file, "CREATE TABLE a (id integer);\n").unwrap();
        let error = unchanged_from_ref("HEAD", &[file]).unwrap_err();
        assert!(error.contains("not in a git repository"), "{error}");
    }
}
