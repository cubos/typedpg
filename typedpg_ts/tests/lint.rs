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
