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
        .args(["--previous-deployment", "HEAD"])
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
fn lint_fix_removes_an_unused_returning_and_reports_the_call_sites() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    // No `lint` section: these rules keep their default levels (the shared
    // fixture turns them off).
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "call_site_fixture"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[package.metadata.typedpg.database]
migrations = "./migrations"
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("migrations/0001_init.sql"),
        "CREATE TABLE users (id integer PRIMARY KEY, org_id integer, name text);\n",
    )
    .unwrap();
    let source = r#"//! The call-site rules, end to end.

use typedpg::sql;

pub async fn run(pool: &typedpg::Pool) -> Result<(), Box<dyn std::error::Error>> {
    sql!(
        pool,
        "SELECT id FROM users WHERE org_id = $org",
        org = &org,
    )
    .fetch_one()
    .await?;
    sql!(
        pool,
        "INSERT INTO users (id, name) VALUES ($id, $name) RETURNING id",
        id = &id,
        name = &name,
    )
    .execute()
    .await?;
    Ok(())
}
"#;
    let path = root.join("src/main.rs");
    std::fs::write(&path, source).unwrap();
    let manifest = root.join("Cargo.toml");
    let run = |fix: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-typedpg"));
        command.args(["typedpg", "lint"]);
        if fix {
            command.arg("--fix");
        }
        command
            .arg("--manifest-path")
            .arg(&manifest)
            .output()
            .expect("run `cargo-typedpg`")
    };

    let output = run(false);
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}\n{stdout}");
    assert!(
        stdout.contains("warning[fetch_may_return_many]"),
        "{stdout}"
    );
    assert!(stdout.contains("info[unused_returning]"), "{stdout}");
    assert!(
        stdout.contains(
            "`.fetch_one()` fails with TooManyRows if this query returns a second row, and \
             nothing in the schema limits it to one"
        ),
        "{stdout}"
    );
    // The findings sit at the methods in the calling file, not in the SQL.
    assert!(stdout.contains("src/main.rs:11:6]"), "{stdout}");
    assert!(stdout.contains("src/main.rs:19:6]"), "{stdout}");

    let output = run(true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fixed 1 finding in 1 file"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r#"//! The call-site rules, end to end.

use typedpg::sql;

pub async fn run(pool: &typedpg::Pool) -> Result<(), Box<dyn std::error::Error>> {
    sql!(
        pool,
        "SELECT id FROM users WHERE org_id = $org",
        org = &org,
    )
    .fetch_one()
    .await?;
    sql!(
        pool,
        "INSERT INTO users (id, name) VALUES ($id, $name)",
        id = &id,
        name = &name,
    )
    .execute()
    .await?;
    Ok(())
}
"#
    );

    let output = run(false);
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(!stdout.contains("unused_returning"), "{stdout}");
    assert!(stdout.contains("fetch_may_return_many"), "{stdout}");
}

#[test]
fn lint_reports_the_call_of_a_bound_query() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "bound_call_fixture"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[package.metadata.typedpg.database]
migrations = "./migrations"
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("migrations/0001_init.sql"),
        "CREATE TABLE users (id integer PRIMARY KEY, org_id integer, name text);\n",
    )
    .unwrap();
    let path = root.join("src/main.rs");
    std::fs::write(
        &path,
        r#"//! A query bound to a local, called through the binding.
use typedpg::sql;

pub async fn run(pool: &typedpg::Pool) -> Result<(), Box<dyn std::error::Error>> {
    let query = sql!(pool, "SELECT id FROM users WHERE org_id = $org", org = &org);
    let _ = query.fetch_one().await?;
    Ok(())
}
"#,
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
    assert!(stdout.contains("src/main.rs:6:19]"), "{stdout}");
    assert!(
        stdout.contains("warning[fetch_may_return_many]"),
        "{stdout}"
    );
}

#[test]
fn an_escaping_bound_query_does_not_report_its_returning() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "escaping_call_fixture"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[package.metadata.typedpg.database]
migrations = "./migrations"
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("migrations/0001_init.sql"),
        "CREATE TABLE users (id integer PRIMARY KEY, name text);\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        r#"//! The query may be used in a way the calls do not show.
use typedpg::sql;

pub async fn run(pool: &typedpg::Pool, cond: bool) -> Result<(), Box<dyn std::error::Error>> {
    let query = sql!(
        pool,
        "INSERT INTO users (id, name) VALUES ($id, $name) RETURNING id",
        id = &id,
        name = &name,
    );
    if cond {
        query.execute().await?;
    } else {
        drop(query);
    }
    Ok(())
}
"#,
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
    assert!(!stdout.contains("unused_returning"), "{stdout}");
}

#[test]
fn a_macro_that_mentions_a_bound_query_does_not_report_its_returning() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "macro_call_fixture"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[package.metadata.typedpg.database]
migrations = "./migrations"
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("migrations/0001_init.sql"),
        "CREATE TABLE users (id integer PRIMARY KEY, name text);\n",
    )
    .unwrap();
    // The other branch hands the query to a macro, whose tokens the scan
    // cannot see into: the RETURNING may be needed there.
    std::fs::write(
        root.join("src/main.rs"),
        r#"//! A macro hides a use of the bound query.
use typedpg::sql;

pub async fn run(pool: &typedpg::Pool, cond: bool) -> Result<(), Box<dyn std::error::Error>> {
    let query = sql!(
        pool,
        "INSERT INTO users (id, name) VALUES ($id, $name) RETURNING id",
        id = &id,
        name = &name,
    );
    if cond {
        query.execute().await?;
    } else {
        println!("{}", fetch_id!(query));
    }
    Ok(())
}
"#,
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
    assert!(!stdout.contains("unused_returning"), "{stdout}");
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

/// Commit everything in `dir`.
fn commit_all(dir: &Path, message: &str) {
    let identity = ["-c", "user.name=Test", "-c", "user.email=test@example.com"];
    git(dir, &[&identity[..], &["add", "-A", "."]].concat());
    git(
        dir,
        &[&identity[..], &["commit", "-q", "-m", message]].concat(),
    );
}

/// A crate whose deployed code (the first commit) has `queries` in
/// `src/lib.rs` and `0001_init.sql`, and whose branch adds `migration` as
/// `0002_change.sql` and replaces the queries with `new_queries`.
fn deployed_crate(queries: &[&str], migration: &str, new_queries: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "deployed_fixture"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[package.metadata.typedpg.database]
migrations = "./migrations"
"#,
    )
    .unwrap();
    let source = |queries: &[&str]| {
        let mut source = String::from(
            "use typedpg::sql;\n\npub async fn run(pool: &typedpg::Pool) -> \
             Result<(), typedpg::Error> {\n",
        );
        for query in queries {
            source.push_str(&format!("    sql!(pool, {query:?}).execute().await?;\n"));
        }
        source.push_str("    Ok(())\n}\n");
        source
    };
    std::fs::write(root.join("src/lib.rs"), source(queries)).unwrap();
    std::fs::write(
        root.join("migrations/0001_init.sql"),
        "CREATE TYPE status AS ENUM ('active', 'blocked');\n\
         CREATE TABLE users (\n\
             id integer PRIMARY KEY,\n\
             email text NOT NULL,\n\
             name text NOT NULL,\n\
             status status NOT NULL,\n\
             age integer\n\
         );\n",
    )
    .unwrap();
    git(root, &["init", "-q"]);
    commit_all(root, "deployed");
    std::fs::write(root.join("migrations/0002_change.sql"), migration).unwrap();
    std::fs::write(root.join("src/lib.rs"), source(new_queries)).unwrap();
    dir
}

/// `cargo typedpg lint` over `dir` with `args`: its exit code, stdout and
/// stderr.
fn lint_crate(dir: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-typedpg"))
        .args(["typedpg", "lint", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .args(args)
        .output()
        .expect("run `cargo-typedpg`");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn breaks_deployed_queries_reports_what_the_deployed_code_trips_on() {
    let dir = deployed_crate(
        &[
            "SELECT email FROM users WHERE id = $id",
            "SELECT name, status FROM users",
            "SELECT * FROM users",
            "UPDATE users SET age = $age WHERE id = 1",
        ],
        "ALTER TABLE users DROP COLUMN email;\n\
         ALTER TABLE users ALTER COLUMN name DROP NOT NULL;\n\
         ALTER TYPE status ADD VALUE 'deleted';\n\
         ALTER TABLE users ALTER COLUMN age TYPE bigint;\n",
        // The branch removed every query: the deployed code still runs them.
        &[],
    );
    let (code, stdout, stderr) = lint_crate(dir.path(), &["--previous-deployment", "HEAD"]);
    assert_eq!(code, Some(0), "{stderr}\n{stdout}");
    let findings = stdout.matches("warning[breaks_deployed_queries]").count();
    // One per query and statement that breaks it.
    assert_eq!(findings, 8, "{stdout}");
    for expected in [
        // No longer analyzes, with PostgreSQL's message; on the statement.
        "migrations/0002_change.sql:1:1",
        "this migration breaks the deployed query at src/lib.rs:4 (HEAD): it no longer runs: \
         column \"email\" does not exist",
        "result column \"name\" may now be NULL",
        "result column \"status\" may now be 'deleted', a value the deployed code does not know",
        "this migration breaks the deployed query at src/lib.rs:6 (HEAD): result column \"email\" \
         is gone",
        "migrations/0002_change.sql:4:1",
        "parameter $age changes from integer to bigint",
        "first deploy code that works with both schemas, then the migration (expand/contract)",
    ] {
        assert!(stdout.contains(expected), "{expected}\n{stdout}");
    }

    // Without the flag, the rule has nothing to compare with.
    let (_, stdout, _) = lint_crate(dir.path(), &[]);
    assert!(!stdout.contains("breaks_deployed_queries"), "{stdout}");

    // The flag's old name still works, and reports the same.
    let (code, alias_stdout, stderr) =
        lint_crate(dir.path(), &["--ignore-migrations-from", "HEAD"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        alias_stdout
            .matches("warning[breaks_deployed_queries]")
            .count(),
        8,
        "{alias_stdout}"
    );
    assert!(
        stderr.contains("(1 unchanged from HEAD: statement rules skipped)"),
        "{stderr}"
    );
}

#[test]
fn breaks_deployed_queries_reports_an_insert_and_honors_its_level() {
    let dir = deployed_crate(
        &["INSERT INTO users (id, email, name, status) VALUES ($id, $email, $name, 'active')"],
        "ALTER TABLE users ADD COLUMN tenant integer NOT NULL;\n",
        &[],
    );
    let (_, stdout, stderr) = lint_crate(dir.path(), &["--previous-deployment", "HEAD"]);
    assert!(
        stdout.contains(
            "null value in column \"tenant\" of relation \"users\" violates not-null constraint"
        ),
        "{stdout}\n{stderr}"
    );

    // `fail_on` and the level apply as to any rule.
    let manifest = dir.path().join("Cargo.toml");
    let mut config = std::fs::read_to_string(&manifest).unwrap();
    config.push_str("\n[package.metadata.typedpg.lint]\nbreaks_deployed_queries = \"error\"\n");
    std::fs::write(&manifest, config).unwrap();
    let (code, stdout, _) = lint_crate(dir.path(), &["--previous-deployment", "HEAD"]);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(
        stdout.contains("error[breaks_deployed_queries]"),
        "{stdout}"
    );
}

#[test]
fn breaks_deployed_queries_accepts_a_compatible_migration() {
    let dir = deployed_crate(
        &[
            "SELECT id, email, name, status, age FROM users WHERE id = $id",
            "INSERT INTO users (id, email, name, status) VALUES ($id, $email, $name, 'active')",
            "UPDATE users SET age = $age WHERE id = $id",
        ],
        // Expand: a nullable column, a defaulted NOT NULL one, a new table
        // and an index change nothing for the deployed code.
        "ALTER TABLE users ADD COLUMN bio text;\n\
         ALTER TABLE users ADD COLUMN score integer NOT NULL DEFAULT 0;\n\
         CREATE TABLE posts (id integer PRIMARY KEY);\n\
         CREATE INDEX users_name_idx ON users (name);\n",
        &["SELECT id, bio, score FROM users"],
    );
    let (code, stdout, stderr) = lint_crate(dir.path(), &["--previous-deployment", "HEAD"]);
    assert_eq!(code, Some(0), "{stderr}\n{stdout}");
    assert!(!stdout.contains("breaks_deployed_queries"), "{stdout}");
}
