//! `typedpg lint --fix`: the SQL fixes of a `.sql` query and of an `sql()`
//! template literal are written into their files, and a second run reports
//! neither.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_typedpg"))
        .args(args)
        .current_dir(root)
        .output()
        .expect("run `typedpg`")
}

#[test]
fn fix_rewrites_sql_files_and_template_literals() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "sql": { "include": ["queries/*.sql"], "emit": "none" }
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);\n",
    );
    let ts = root.join("src/app.ts");
    write(
        &ts,
        "import { sql } from './db';\nsql(`SELECT DISTINCT id FROM users`);\n",
    );
    let sql = root.join("queries/ids.sql");
    write(
        &sql,
        "/* @name ids */\nSELECT id FROM users WHERE id = $id LIMIT 1;\n",
    );

    // The generated module has to exist for `./db` to resolve.
    let generated = run(root, &["gen"]);
    assert!(
        generated.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&generated.stdout),
        String::from_utf8_lossy(&generated.stderr)
    );

    let output = run(root, &["lint", "--fix"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("fixed 2 findings in 2 files"), "{stderr}");
    assert_eq!(
        fs::read_to_string(&ts).unwrap(),
        "import { sql } from './db';\nsql(`SELECT id FROM users`);\n"
    );
    assert_eq!(
        fs::read_to_string(&sql).unwrap(),
        "/* @name ids */\nSELECT id FROM users WHERE id = $id;\n"
    );

    // Nothing left to fix.
    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("redundant_limit"), "{stdout}");
    assert!(!stdout.contains("redundant_distinct"), "{stdout}");
    assert!(!stderr.contains("fixed"), "{stderr}");
}

/// Generate the modules the sources import; panic with the CLI's output when
/// it fails.
fn generate(root: &Path) {
    let output = run(root, &["gen"]);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn call_site_findings_are_placed_at_the_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "sql": { "include": ["src/*.sql"] }
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, org_id int4 NOT NULL, name text NOT NULL);\n",
    );
    write(
        &root.join("src/queries.sql"),
        "/* @name userById */\nSELECT id FROM users WHERE org_id = $org;\n",
    );
    write(
        &root.join("src/exported.ts"),
        r#"import { sql } from './db';

export const exportedQuery = sql(`SELECT id FROM users WHERE org_id = $org`);
export const exportedExecute = sql(`INSERT INTO users (id, org_id, name) VALUES ($id, $org, $name) RETURNING id`);
"#,
    );
    let app = concat!(
        "import { sql } from './db';\n",
        "import { userById } from './queries';\n",
        "import { userById as byId } from './queries';\n",
        "import * as queries from './queries';\n",
        "import { exportedQuery } from './exported';\n",
        "import { exportedQuery as aliased } from './exported';\n",
        "import * as all from './exported';\n",
        "\n",
        "sql(`SELECT id FROM users WHERE org_id = $org`).fetchOne(pool, { org });\n",
        "const local = sql(`SELECT id FROM users WHERE org_id = $org`);\n",
        "local.fetchOptional(pool, { org });\n",
        "userById.fetchOne(pool, { org });\n",
        "byId.fetchOptional(pool, { org });\n",
        "queries.userById.fetchAll(pool, { org });\n",
        "exportedQuery.fetchOne(pool, { org });\n",
        "aliased.fetchOptional(pool, { org });\n",
        "all.exportedQuery.fetchAll(pool, { org });\n",
        "exportedExecute.execute(pool, { id, org, name });\n",
    );
    write(&root.join("src/app.ts"), app);
    generate(root);

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    // Every offending call, named as written, at its own line.
    for (line, method) in [
        (9, "fetchOne"),
        (11, "fetchOptional"),
        (12, "fetchOne"),
        (13, "fetchOptional"),
        (15, "fetchOne"),
        (16, "fetchOptional"),
    ] {
        let at = format!("src/app.ts:{line}:");
        assert!(stdout.contains(&at), "{stdout}");
        assert!(
            stdout.contains(&format!("`.{method}()` fails with TooManyRows")),
            "{stdout}"
        );
    }
    assert_eq!(
        stdout.matches("warning[fetch_may_return_many]").count(),
        6,
        "{stdout}"
    );
    // An exported query can be called anywhere: its execute does not report
    // the unused RETURNING.
    assert!(!stdout.contains("unused_returning"), "{stdout}");
}

#[test]
fn fix_removes_returning_of_a_direct_execute() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts"
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);\n",
    );
    let app = root.join("src/app.ts");
    write(
        &app,
        "import { sql } from './db';\n\
         sql(`INSERT INTO users (id, name) VALUES ($id, $name) RETURNING id`)\n\
             .execute(pool, { id, name });\n",
    );
    generate(root);

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("src/app.ts:3:2]"), "{stdout}");
    assert!(stdout.contains("info[unused_returning]"), "{stdout}");
    assert!(
        stdout.contains("`.execute()` discards the RETURNING rows"),
        "{stdout}"
    );

    let output = run(root, &["lint", "--fix"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("fixed 1 finding in 1 file"), "{stderr}");
    assert_eq!(
        fs::read_to_string(&app).unwrap(),
        "import { sql } from './db';\n\
         sql(`INSERT INTO users (id, name) VALUES ($id, $name)`)\n\
             .execute(pool, { id, name });\n"
    );

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("unused_returning"), "{stdout}");
}

#[test]
fn inline_calls_are_linted_before_the_module_is_generated() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "include": ["src"], "out": "src/generated/db.ts" }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, org_id int4, name text NOT NULL);\n",
    );
    write(
        &root.join("src/app/users.ts"),
        "import { sql } from '../generated/db.js';\n\
         sql(`INSERT INTO users (id, name) VALUES ($id, $name) RETURNING id`).execute(pool, { id, name });\n\
         sql(`SELECT id FROM users WHERE org_id = $org`).fetchOne(pool, { org });\n",
    );
    write(
        &root.join("src/app/broken.ts"),
        "import { sql } from '../generated/db';\nsql(`SELECT nope FROM users`);\n",
    );

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The module doesn't exist, and lint doesn't write it ...
    assert!(!root.join("src/generated").exists());
    // ... but the calls on `sql()` imported from it are linted all the same.
    assert!(stderr.contains("checked 3 queries"), "{stderr}");
    assert!(stdout.contains("src/app/users.ts:2:70]"), "{stdout}");
    assert!(stdout.contains("info[unused_returning]"), "{stdout}");
    assert!(stdout.contains("src/app/users.ts:3:49]"), "{stdout}");
    assert!(
        stdout.contains("warning[fetch_may_return_many]"),
        "{stdout}"
    );
    // A query that doesn't analyze fails the run, as it fails `gen`.
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("column \"nope\" does not exist"),
        "{stdout}\n{stderr}"
    );
}

#[test]
fn an_invalid_lint_key_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "include": ["src"], "out": "src/db.ts", "lint": { "bogusRule": "error" } }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE t (id int4);\n",
    );
    for format in ["human", "json", "sarif"] {
        let output = run(root, &["lint", "--format", format]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{format}: {stdout}\n{stderr}"
        );
        assert!(
            format!("{stdout}{stderr}").contains("unknown rule `bogusRule` in `lint`"),
            "{format}: {stdout}\n{stderr}"
        );
        assert!(
            stderr.contains("typedpg lint: invalid configuration, nothing was checked"),
            "{format}: {stderr}"
        );
        assert!(!stderr.contains("passed"), "{format}: {stderr}");
    }
    // The JSON document carries the error as its failure.
    let output = run(root, &["lint", "--format", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["failures"][0]["path"], "typedpg.config.json");
}

#[test]
fn a_sql_file_imported_directly_is_linted_at_the_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "sql": { "include": ["queries/*.sql"], "emit": "none" }
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, org_id int4 NOT NULL);\n",
    );
    write(
        &root.join("queries/users.sql"),
        "/* @name userById */\nSELECT id FROM users WHERE org_id = $org;\n",
    );
    write(
        &root.join("src/app.ts"),
        "import { userById } from '../queries/users.sql';\n\
         userById.fetchOptional(pool, { org });\n",
    );
    generate(root);

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("src/app.ts:2:10]"), "{stdout}");
    assert!(
        stdout.contains("warning[fetch_may_return_many]"),
        "{stdout}"
    );
    assert!(
        stdout.contains("`.fetchOptional()` fails with TooManyRows"),
        "{stdout}"
    );
}

#[test]
fn a_sql_only_project_sees_its_sources_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "sql": { "include": ["queries/*.sql"], "emit": "none" }
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, org_id int4 NOT NULL);
",
    );
    write(
        &root.join("queries/users.sql"),
        "/* @name userById */
SELECT id FROM users WHERE org_id = $org;
",
    );
    write(
        &root.join("src/app.ts"),
        "import { userById } from '../queries/users.sql';\n\
         userById.fetchOne(pool, { org });\n",
    );

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("src/app.ts:2:10]"), "{stdout}");
    assert!(
        stdout.contains("warning[fetch_may_return_many]"),
        "{stdout}"
    );
}

#[test]
fn a_sql_file_imported_through_its_declaration_is_linted_at_the_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "sql": { "include": ["queries/*.sql"], "emit": "dts" }
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, org_id int4 NOT NULL);\n",
    );
    write(
        &root.join("queries/users.sql"),
        "/* @name userById */\nSELECT id FROM users WHERE org_id = $org;\n",
    );
    write(
        &root.join("src/app.ts"),
        "import { userById } from '../queries/users.sql';\n\
         userById.fetchOne(pool, { org });\n",
    );
    generate(root);

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("src/app.ts:2:10]"), "{stdout}");
    assert!(
        stdout.contains("warning[fetch_may_return_many]"),
        "{stdout}"
    );
}

#[test]
fn a_mutable_module_export_does_not_attribute_remote_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts"
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY);\n",
    );
    write(
        &root.join("src/a.ts"),
        "import { sql } from './db';\n\
         export let mutable = sql(`SELECT 1 AS id`);\n\
         export const one = sql(`SELECT 1 AS id`);\n\
         export function replace() { mutable = sql(`SELECT id FROM users`); }\n",
    );
    write(
        &root.join("src/b.ts"),
        "import { mutable, one, replace } from './a';\n\
         replace();\n\
         mutable.fetchAll(db);\n\
         one.fetchAll(db);\n",
    );
    generate(root);

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    // The `const` export names the single-row query; the `let` one may have
    // been replaced by `replace()` before the call, so it is not known to be
    // that query.
    assert_eq!(
        stdout.matches("fetch_all_single_row]").count(),
        1,
        "{stdout}"
    );
    assert!(stdout.contains("src/b.ts:4:"), "{stdout}");
    assert!(!stdout.contains("src/b.ts:3:"), "{stdout}");
}

#[test]
fn an_escaping_bound_query_does_not_report_its_returning() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts"
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);\n",
    );
    let app = root.join("src/app.ts");
    write(
        &app,
        "import { sql } from './db';\n\
         const query = sql(`INSERT INTO users (id, name) VALUES ($id, $name) RETURNING id`);\n\
         consume(query);\n\
         query.execute(pool, { id, name });\n",
    );
    generate(root);

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("unused_returning"), "{stdout}");
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|error| panic!("git {}: {error}", args.join(" ")));
    assert!(status.success(), "git {} failed", args.join(" "));
}

#[test]
fn breaks_deployed_queries_checks_the_previous_deployments_queries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "sql": { "include": ["queries/*.sql"], "emit": "none" }
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL, email text NOT NULL);\n",
    );
    write(
        &root.join("src/app.ts"),
        "import { sql } from './db';\nexport const names = sql(`SELECT name FROM users`);\n",
    );
    write(
        &root.join("queries/users.sql"),
        "/* @name email */\nSELECT email FROM users WHERE id = $id;\n",
    );
    // The generated module is not committed: the deployed checkout gets an
    // empty one, as `typedpg gen` would write first.
    write(&root.join(".gitignore"), "src/db.ts\n");
    generate(root);
    git(root, &["init", "-q"]);
    git(root, &["add", "-A", "."]);
    git(root, &["commit", "-q", "-m", "deployed"]);

    // The branch drops a column and stops reading it; the deployed code
    // still does.
    write(
        &root.join("migrations/0002.sql"),
        "ALTER TABLE users DROP COLUMN email;\n\
         ALTER TABLE users ALTER COLUMN name DROP NOT NULL;\n",
    );
    write(
        &root.join("queries/users.sql"),
        "/* @name byId */\nSELECT id FROM users WHERE id = $id;\n",
    );
    generate(root);

    for flag in ["--previous-deployment", "--ignore-migrations-from"] {
        let output = run(root, &["lint", flag, "HEAD"]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert_eq!(
            stdout.matches("warning[breaks_deployed_queries]").count(),
            2,
            "{stdout}"
        );
        for expected in [
            "this migration breaks the deployed query at queries/users.sql:2 (HEAD): it no \
             longer runs: column \"email\" does not exist",
            "migrations/0002.sql:1:1",
            "this migration breaks the deployed query at src/app.ts:2 (HEAD): result column \
             \"name\" may now be NULL",
            "migrations/0002.sql:2:1",
        ] {
            assert!(stdout.contains(expected), "{expected}\n{stdout}");
        }
        assert!(
            stderr.contains("(1 unchanged from HEAD: statement rules skipped)"),
            "{stderr}"
        );
    }

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("breaks_deployed_queries"), "{stdout}");
}

#[test]
fn queries_are_linted_as_the_databases_roles() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "roles": ["app"],
            "sql": { "include": ["queries/*.sql"], "emit": "none" }
        }"#,
    );
    // `app`'s search path finds `app_data.t`, whose `name` may be NULL:
    // the check is not redundant for it.
    write(
        &root.join("migrations/0001.sql"),
        "CREATE ROLE app;\n\
         CREATE SCHEMA app_data;\n\
         CREATE TABLE t (id int4 PRIMARY KEY, name text NOT NULL);\n\
         CREATE TABLE app_data.t (id int4 PRIMARY KEY, name text);\n\
         GRANT USAGE ON SCHEMA app_data TO app;\n\
         GRANT SELECT ON t, app_data.t TO app;\n\
         ALTER ROLE app SET search_path = app_data, public;\n",
    );
    write(
        &root.join("queries/named.sql"),
        "/* @name named */\nSELECT id FROM t WHERE name IS NOT NULL;\n",
    );
    write(&root.join("src/app.ts"), "export {};\n");
    generate(root);

    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("redundant_null_check"), "{stdout}");

    // Without the role, the plain analysis reads `public.t`.
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "sql": { "include": ["queries/*.sql"], "emit": "none" }
        }"#,
    );
    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("redundant_null_check"), "{stdout}");
}

#[test]
fn breaks_deployed_queries_checks_the_previous_deployments_copy_in_targets() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "roles": ["app"]
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE ROLE app;\n\
         CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL, age int4);\n\
         CREATE TABLE events (id int4 PRIMARY KEY, kind text NOT NULL);\n\
         GRANT INSERT ON users, events TO app;\n",
    );
    write(
        &root.join("src/app.ts"),
        "import { copyIn } from './db';\n\
         export const users = copyIn(\"users (id, name, age)\");\n\
         export const events = copyIn(\"events (id, kind)\");\n",
    );
    write(&root.join(".gitignore"), "src/db.ts\n");
    generate(root);
    git(root, &["init", "-q"]);
    git(root, &["add", "-A", "."]);
    git(root, &["commit", "-q", "-m", "deployed"]);

    // The branch limits `age` and takes INSERT on `events` from the role the
    // code runs as; it no longer copies into either table, the deployed code
    // still does.
    write(
        &root.join("migrations/0002.sql"),
        "ALTER TABLE users ADD CONSTRAINT users_age_positive CHECK (age > 0);\n\
         REVOKE INSERT ON events FROM app;\n",
    );
    write(&root.join("src/app.ts"), "export {};\n");
    generate(root);

    let output = run(root, &["lint", "--previous-deployment", "HEAD"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        stdout.matches("warning[breaks_deployed_queries]").count(),
        2,
        "{stdout}"
    );
    for expected in [
        "this migration breaks the deployed query at src/app.ts:2 (HEAD): its COPY into users \
         writes column age, which CHECK constraint \"users_age_positive\" now limits it to at \
         least 1: writing another value fails",
        "migrations/0002.sql:1:1",
        "this migration breaks the deployed query at src/app.ts:3 (HEAD): it no longer runs as \
         role \"app\": permission denied for table events",
        "migrations/0002.sql:2:1",
    ] {
        assert!(stdout.contains(expected), "{expected}\n{stdout}");
    }
}

/// A tsconfig.json `paths` alias for the generated module, inherited through
/// `extends`, with the `baseUrl` the base sets.
fn aliased_project(root: &Path) {
    write(
        &root.join("typedpg.config.json"),
        r#"{ "include": ["src"], "out": "src/generated/queries.ts" }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);\n",
    );
    write(
        &root.join("tsconfig.base.json"),
        r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@db": ["./src/generated/queries"],
                    "@gen/*": ["src/generated/*"]
                }
            }
        }"#,
    );
    write(
        &root.join("tsconfig.json"),
        r#"{ "extends": "./tsconfig.base.json", "include": ["src"] }"#,
    );
    write(
        &root.join("src/app.ts"),
        "import { sql } from '@db';\n\
         import * as db from '@gen/queries';\n\
         import { sql as viaBaseUrl } from 'src/generated/queries';\n\
         sql(`SELECT DISTINCT id FROM users`);\n\
         db.sql(`SELECT DISTINCT id, name FROM users`);\n\
         viaBaseUrl(`SELECT id FROM users LIMIT 1 OFFSET 0`);\n",
    );
}

#[test]
fn queries_imported_through_tsconfig_paths_are_linted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    aliased_project(root);

    let check = |when: &str| {
        let output = run(root, &["lint"]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{when}: {stdout}\n{stderr}");
        assert!(stderr.contains("checked 3 queries"), "{when}: {stderr}");
        assert_eq!(
            stdout.matches("info[redundant_distinct]").count(),
            2,
            "{when}: {stdout}"
        );
        for expected in ["src/app.ts:4:13", "src/app.ts:5:16", "src/app.ts:6:"] {
            assert!(stdout.contains(expected), "{when}: {expected}\n{stdout}");
        }
    };
    // Before `gen`: the generated module doesn't exist, the aliases still
    // resolve to it.
    check("before gen");
    assert!(!root.join("src/generated/queries.ts").exists());

    // `gen` generates the aliased queries, and lint still sees them.
    generate(root);
    let module = fs::read_to_string(root.join("src/generated/queries.ts")).unwrap();
    assert!(module.contains("SELECT DISTINCT id FROM users"), "{module}");
    assert!(
        module.contains("SELECT id FROM users LIMIT 1 OFFSET 0"),
        "{module}"
    );
    check("after gen");
}

/// Every comment of a `.sql` file belongs to a query, for its allow comments
/// to suppress and be checked: the comments on the line a statement ends
/// on, after it (and its `;`), are that statement's; the others lead into
/// the next query; those after the last query are its.
#[test]
fn allow_comments_around_sql_file_queries_belong_to_a_query() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "sql": { "include": ["queries/*.sql"], "emit": "none" }
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE posts (id int4 PRIMARY KEY);\n",
    );
    write(
        &root.join("queries/posts.sql"),
        "-- typedpg-lint: allow(header_rule)\n\
         /* @name a */\n\
         SELECT id AS id FROM posts; -- typedpg-lint: allow(redundant_alias)\n\
         \n\
         /* @name b */\n\
         SELECT id AS id FROM posts; -- typedpg-lint: allow(trailing_rule)\n\
         \n\
         -- typedpg-lint: allow(redundant_alias)\n\
         /* @name c */\n\
         SELECT id AS id FROM posts;\n\
         \n\
         /* @name d */\n\
         SELECT id AS id FROM posts /* typedpg-lint: allow(redundant_alias) */\n\
         -- typedpg-lint: allow(last_rule)\n",
    );
    write(&root.join("src/app.ts"), "export {};\n");
    generate(root);

    let output = run(root, &["lint", "--format", "json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut found: Vec<(String, u64)> = json["findings"]
        .as_array()
        .unwrap_or_else(|| panic!("{stdout}"))
        .iter()
        .map(|finding| {
            (
                finding["rule"].as_str().unwrap().to_owned(),
                finding["line"].as_u64().unwrap(),
            )
        })
        .collect();
    found.sort();
    assert_eq!(
        found,
        [
            ("invalid_allow".to_owned(), 1),
            ("invalid_allow".to_owned(), 6),
            ("invalid_allow".to_owned(), 14),
            ("redundant_alias".to_owned(), 6),
        ],
        "{stdout}"
    );
}

#[test]
fn json_stringify_param_is_reported_at_the_stringify_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "sql": { "include": ["src/*.sql"] }
        }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE DOMAIN prefs AS jsonb;\n\
         CREATE TABLE docs (id int4 PRIMARY KEY, body jsonb NOT NULL, raw json, \
         settings prefs, tags jsonb[], title text);\n",
    );
    write(
        &root.join("src/queries.sql"),
        "/* @name saveDoc */\nUPDATE docs SET body = $body WHERE id = $id;\n\n\
         /* @name saveAllowed */\n-- typedpg-lint: allow(json_stringify_param)\n\
         UPDATE docs SET body = $body WHERE id = $id;\n",
    );
    let app = concat!(
        "import { sql, copyIn } from './db';\n",
        "import { saveDoc, saveAllowed } from './queries';\n",
        "\n",
        "sql(`UPDATE docs SET body = $body WHERE id = $id`).execute(pool, { id, body: JSON.stringify(doc) });\n",
        "const q = sql(`UPDATE docs SET raw = $raw, settings = $settings WHERE id = $id`);\n",
        "q.execute(pool, { id, raw: (JSON.stringify(doc) as string), settings: JSON.stringify(doc)! });\n",
        "sql(`UPDATE docs SET tags = $tags WHERE id = $id`).execute(pool, { id, tags: [JSON.stringify(doc)] });\n",
        "sql(`INSERT INTO docs (id, body) VALUES $..rows { id, body }`).execute(pool, { rows: [{ id, body: JSON.stringify(doc) }] });\n",
        "saveDoc.execute(pool, { id, body: JSON.stringify(doc) });\n",
        "copyIn(`docs (id, body)`).execute(pool, [{ id, body: JSON.stringify(doc) }]);\n",
        // Not reported: a text column, the value itself, a value from a
        // variable, the allowed query.
        "sql(`UPDATE docs SET title = $title WHERE id = $id`).execute(pool, { id, title: JSON.stringify(doc) });\n",
        "sql(`UPDATE docs SET body = $body WHERE id = $id`).execute(pool, { id, body: doc });\n",
        "const text = JSON.stringify(doc);\n",
        "sql(`UPDATE docs SET body = $body WHERE id = $id`).execute(pool, { id, body: text });\n",
        "saveAllowed.execute(pool, { id, body: JSON.stringify(doc) });\n",
    );
    write(&root.join("src/app.ts"), app);
    generate(root);

    let output = run(root, &["lint", "--format", "json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // An error by default: the run fails.
    assert!(!output.status.success(), "{stdout}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let found: Vec<(u64, u64, u64, String)> = json["findings"]
        .as_array()
        .unwrap_or_else(|| panic!("{stdout}"))
        .iter()
        .filter(|finding| finding["rule"] == "json_stringify_param")
        .map(|finding| {
            assert_eq!(finding["severity"], "error", "{stdout}");
            (
                finding["line"].as_u64().unwrap(),
                finding["column"].as_u64().unwrap(),
                finding["endColumn"].as_u64().unwrap(),
                finding["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let message = |subject: &str| {
        format!("{subject}, and `JSON.stringify(…)` stores a JSON string, not the value")
    };
    // Each finding spans the `JSON.stringify(doc)` call (19 characters).
    let at = |line: u64, column: u64, subject: &str| (line, column, column + 19, message(subject));
    assert_eq!(
        found,
        [
            at(4, 78, "parameter $body is jsonb"),
            at(6, 29, "parameter $raw is json"),
            at(
                6,
                71,
                "parameter $settings is public.prefs, a domain over jsonb"
            ),
            at(7, 79, "an element of parameter $tags is jsonb"),
            at(8, 99, "field body of $..rows is jsonb"),
            at(9, 35, "parameter $body is jsonb"),
            at(10, 54, "column body is jsonb"),
        ],
        "{stdout}"
    );

    // Off in the configuration, nothing is reported.
    write(
        &root.join("typedpg.config.json"),
        r#"{
            "include": ["src"],
            "out": "src/db.ts",
            "sql": { "include": ["src/*.sql"] },
            "lint": { "jsonStringifyParam": "off" }
        }"#,
    );
    let output = run(root, &["lint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("json_stringify_param"), "{stdout}");
}
