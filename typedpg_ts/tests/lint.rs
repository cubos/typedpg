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
