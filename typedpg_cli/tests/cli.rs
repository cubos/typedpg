//! `cargo typedpg migrate …` end to end: the binary run in a scratch
//! project directory against the PostgreSQL server `typedpg_test_support`
//! shares across the test run (TLS on, with a self-signed certificate).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const MANIFEST: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
                        [package.metadata.typedpg.database]\nmigrations = \"./db\"\n";

/// A scratch project: a Cargo.toml pointing at `./db`, with the given
/// migration files.
fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Cargo.toml"), MANIFEST).unwrap();
    fs::create_dir(dir.path().join("db")).unwrap();
    for (name, sql) in files {
        fs::write(dir.path().join("db").join(name), sql).unwrap();
    }
    dir
}

fn cli(dir: &Path, url: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cargo-typedpg"));
    cmd.arg("typedpg").args(args).current_dir(dir);
    match url {
        Some(url) => cmd.env("DATABASE_URL", url),
        None => cmd.env_remove("DATABASE_URL"),
    };
    cmd.output().expect("run cargo-typedpg")
}

#[track_caller]
fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "exit {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[track_caller]
fn failed(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(1), "expected a failure: {out:?}");
    String::from_utf8(out.stderr.clone()).unwrap()
}

async fn fresh_url() -> String {
    let server = typedpg_test_support::server();
    server.url(&server.create_database("cli").await)
}

#[test]
fn create_writes_timestamped_up_and_down_files() {
    let dir = project(&[]);
    fs::remove_dir(dir.path().join("db")).unwrap();

    ok(&cli(dir.path(), None, &["migrate", "create", "add_users"]));

    let mut files: Vec<String> = fs::read_dir(dir.path().join("db"))
        .expect("create makes the configured directory")
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(files.len(), 2, "{files:?}");
    let (down, up) = (&files[0], &files[1]);
    let stamp = up.strip_suffix("_add_users.sql").expect("up file name");
    assert!(
        stamp.len() == 14 && stamp.bytes().all(|b| b.is_ascii_digit()),
        "{up}"
    );
    assert_eq!(*down, format!("{stamp}_add_users.down.sql"));
    for f in &files {
        assert_eq!(
            fs::read_to_string(dir.path().join("db").join(f)).unwrap(),
            ""
        );
    }
    // The new migration parses as a valid source.
    typedpg::migrate::MigrationSource::from_dir(&dir.path().join("db")).unwrap();
}

#[test]
fn database_commands_need_database_url() {
    let dir = project(&[]);
    let stderr = failed(&cli(dir.path(), None, &["migrate", "status"]));
    assert_eq!(
        stderr,
        "Error: DATABASE_URL environment variable must be set\n"
    );
}

#[tokio::test]
async fn up_status_and_down() {
    let url = fresh_url().await;
    let dir = project(&[
        ("0001_users.sql", "CREATE TABLE users (id INT);"),
        ("0001_users.down.sql", "DROP TABLE users;"),
        ("0002_posts.sql", "CREATE TABLE posts (id INT);"),
        ("0002_posts.down.sql", "DROP TABLE posts;"),
    ]);
    let run = |args: &[&str]| cli(dir.path(), Some(&url), args);

    let status = ok(&run(&["migrate", "status"]));
    assert_eq!(
        status,
        format!(
            "  \u{b7} {:<30} (pending)\n  \u{b7} {:<30} (pending)\n",
            "0001_users", "0002_posts"
        )
    );

    assert_eq!(
        ok(&run(&["migrate", "up"])),
        "Applying 0001_users... done\nApplying 0002_posts... done\nApplied 2 migration(s)\n"
    );
    assert_eq!(ok(&run(&["migrate", "up"])), "No pending migrations\n");

    let status = ok(&run(&["migrate", "status"]));
    let lines: Vec<_> = status.lines().collect();
    assert_eq!(lines.len(), 2, "{status}");
    for (line, name) in lines.iter().zip(["0001_users", "0002_posts"]) {
        assert!(
            line.starts_with(&format!("  \u{2713} {name:<30} (applied 2"))
                && line.ends_with(" UTC)"),
            "{line}"
        );
    }

    // Without a name, down reverts the last applied migration.
    assert_eq!(
        ok(&run(&["migrate", "down"])),
        "Reverting 0002_posts... done\n"
    );
    assert_eq!(
        ok(&run(&["migrate", "down", "0001_users"])),
        "Reverting 0001_users... done\n"
    );
    assert_eq!(
        failed(&run(&["migrate", "down"])),
        "Error: No applied migrations to revert\n"
    );

    // Editing an applied migration shows in status, and blocks up.
    ok(&run(&["migrate", "up"]));
    fs::write(
        dir.path().join("db/0001_users.sql"),
        "CREATE TABLE users (id BIGINT);",
    )
    .unwrap();
    let status = ok(&run(&["migrate", "status"]));
    assert!(
        status.lines().next().unwrap().ends_with("[MODIFIED]"),
        "{status}"
    );
    let stderr = failed(&run(&["migrate", "up"]));
    assert!(
        stderr.contains("migration '0001_users' has been modified since it was applied"),
        "{stderr}"
    );
}

/// The server has TLS on with a self-signed certificate the client cannot
/// verify. `prefer` (also the default) must fall back to a plain
/// connection, as libpq does; `require` must fail rather than go plain.
#[tokio::test]
async fn sslmode_is_honored() {
    let url = fresh_url().await;
    let dir = project(&[("0001_users.sql", "CREATE TABLE users (id INT);")]);
    let status = |url: String| cli(dir.path(), Some(&url), &["migrate", "status"]);

    for query in ["", "?sslmode=prefer", "?sslmode=disable"] {
        let out = ok(&status(format!("{url}{query}")));
        assert!(out.contains("0001_users"), "{query}: {out}");
    }

    let stderr = failed(&status(format!("{url}?sslmode=require")));
    assert!(stderr.contains("TLS"), "{stderr}");

    let stderr = failed(&status(format!("{url}?sslmode=verify-full")));
    assert!(
        stderr.starts_with("Error: invalid DATABASE_URL:"),
        "{stderr}"
    );
}
