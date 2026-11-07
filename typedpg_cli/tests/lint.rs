//! `cargo typedpg lint` over `tests/lint_fixture`, in every format. The
//! expected documents are checked in; run with `BLESS=1` to rewrite them.
//!
//! The fixture is never built: the test only runs the CLI over its sources
//! and migrations.

use std::path::Path;
use std::process::Command;

fn bless() -> bool {
    ["BLESS", "TYPEDPG_BLESS"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty() && s != "0"))
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|error| panic!("git {}: {error}", args.join(" ")));
    assert!(status.success(), "git {} failed", args.join(" "));
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[test]
fn lint_ignores_migrations_unchanged_from_a_ref() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = crate_dir.join("tests/lint_fixture");
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixture, dir.path());
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
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
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    );

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-typedpg"))
        .args(["typedpg", "lint", "--manifest-path"])
        .arg(dir.path().join("Cargo.toml"))
        .args(["--ignore-migrations-from", "HEAD"])
        .output()
        .expect("run `cargo-typedpg`");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    // Every migration is unchanged from HEAD, so none of their statement
    // findings is reported...
    assert!(!stdout.contains("create_index_blocking"), "{stdout}");
    assert!(!stdout.contains("constraint_validating"), "{stdout}");
    // ...but the schema rules still report the current schema's problems,
    // with the fix in a new migration.
    assert!(stdout.contains("fk_without_index"), "{stdout}");
    assert!(
        stdout.contains(
            "CREATE INDEX CONCURRENTLY ON public.posts (author_id), in a new migration whose \
             first line is -- no-transaction"
        ),
        "{stdout}"
    );
    // ...while the queries are still linted, and failures are never dropped.
    assert!(stdout.contains("unbounded_write"), "{stdout}");
    assert!(stdout.contains("tests/ignored.rs:9:24"), "{stdout}");
    assert!(
        stderr.contains(
            "checked 11 queries and 4 migrations (4 unchanged from HEAD: statement rules skipped)"
        ),
        "{stderr}"
    );
}

#[test]
fn lint_ignore_keeps_a_migrations_schema_findings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "lint_ignore_fixture"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[package.metadata.typedpg.database]
migrations = "./migrations"

[package.metadata.typedpg.lint]
ignore = ["migrations/0002_hazards.sql"]
"#,
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "").unwrap();
    std::fs::write(
        root.join("migrations/0001_init.sql"),
        "CREATE TABLE users (id integer PRIMARY KEY);\n\
         CREATE TABLE posts (id integer PRIMARY KEY, author_id integer);\n",
    )
    .unwrap();
    std::fs::write(
        root.join("migrations/0002_hazards.sql"),
        "CREATE INDEX users_id_idx ON users (id);\n\
         ALTER TABLE posts ADD CONSTRAINT posts_author_fk\n\
             FOREIGN KEY (author_id) REFERENCES users (id);\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-typedpg"))
        .args(["typedpg", "lint", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("run `cargo-typedpg`");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}\n{stdout}");
    // The statement rules of the ignored migration are skipped...
    assert!(!stdout.contains("create_index_blocking"), "{stdout}");
    // ...but the schema rules still report, with the fix in a new migration.
    assert!(stdout.contains("warning[fk_without_index]"), "{stdout}");
    assert!(
        stdout.contains(
            "CREATE INDEX CONCURRENTLY ON public.posts (author_id), in a new migration whose \
             first line is -- no-transaction"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("info[redundant_index]"), "{stdout}");
    assert!(
        stdout.contains(
            "DROP INDEX CONCURRENTLY users_id_idx, in a new migration whose first line is \
             -- no-transaction"
        ),
        "{stdout}"
    );
}

#[test]
fn lint_fix_rewrites_the_binding_rules() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = crate_dir.join("tests/lint_fixture");
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixture, dir.path());
    // The fixture's own source also has a binding the fix would rewrite;
    // this test isolates the two in the file it writes.
    std::fs::remove_file(dir.path().join("src/main.rs")).unwrap();
    let source = r#"//! The binding rules' fix, end to end.

use typedpg::sql;

pub async fn insert(tx: &typedpg::Transaction<'_>) -> Result<(), Box<dyn std::error::Error>> {
    sql!(
        tx,
        "INSERT INTO users (id, name) \
         VALUES ($id, $name) \
         ON CONFLICT (id) DO UPDATE \
            SET name = EXCLUDED.name",
        id = id,
        name = next,
    )
    .execute()
    .await?;
    Ok(())
}
"#;
    let path = dir.path().join("src/bindings.rs");
    std::fs::write(&path, source).unwrap();
    let manifest = dir.path().join("Cargo.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-typedpg"))
        .args(["typedpg", "lint", "--fix", "--manifest-path"])
        .arg(&manifest)
        .output()
        .expect("run `cargo-typedpg`");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fixed 2 findings in 1 file"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r#"//! The binding rules' fix, end to end.

use typedpg::sql;

pub async fn insert(tx: &typedpg::Transaction<'_>) -> Result<(), Box<dyn std::error::Error>> {
    sql!(
        tx,
        "INSERT INTO users (id, name) \
         VALUES ($id, $next) \
         ON CONFLICT (id) DO UPDATE \
            SET name = EXCLUDED.name",
    )
    .execute()
    .await?;
    Ok(())
}
"#
    );

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-typedpg"))
        .args(["typedpg", "lint", "--manifest-path"])
        .arg(&manifest)
        .output()
        .expect("run `cargo-typedpg`");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(!stdout.contains("redundant_binding"), "{stdout}");
    assert!(!stdout.contains("renaming_binding"), "{stdout}");
}

#[test]
fn lint_fixture_reports_the_expected_documents() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = crate_dir.join("tests/lint_fixture");
    for (format, expected) in [
        ("human", "expected.txt"),
        ("json", "expected.json"),
        ("sarif", "expected.sarif"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-typedpg"))
            .args(["typedpg", "lint", "--manifest-path"])
            .arg(fixture.join("Cargo.toml"))
            .args(["--format", format])
            .output()
            .unwrap_or_else(|error| panic!("run `cargo-typedpg` for {format}: {error}"));
        let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{format}: expected exit status 1, got {:?}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            !stderr.contains(": error:"),
            "{format}: unexpected failure on stderr:\n{stderr}"
        );
        // The summary block: its timing is the only non-deterministic part,
        // so check the shape without pinning it.
        let lines: Vec<&str> = stderr.lines().collect();
        assert!(lines[0].starts_with("typedpg lint: "), "{format}: {stderr}");
        let checked = lines
            .iter()
            .find(|line| line.starts_with("  checked "))
            .unwrap_or_else(|| panic!("{format}: no checked line in:\n{stderr}"));
        assert!(checked.ends_with('s'), "{format}: {checked}");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("  schema (default): ")),
            "{format}: {stderr}"
        );
        assert!(
            lines
                .last()
                .is_some_and(|line| line.starts_with("  ✖ failed: ")),
            "{format}: {stderr}"
        );
        let path = fixture.join(expected);
        if bless() {
            std::fs::write(&path, &stdout).expect("write the expected document");
        } else {
            let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!(
                    "read {}: {error} — run with BLESS=1 to create it",
                    path.display()
                )
            });
            assert_eq!(
                stdout,
                expected,
                "{format}: stdout differs from {} — run with BLESS=1 to accept it",
                path.display()
            );
        }
    }
}
