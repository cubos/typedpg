//! Whole projects through [`Project`]: how sources reach the generated
//! modules (re-exports, several databases, JavaScript outputs), what
//! `check` does, and the embedded migrations.

use std::fs;
use std::path::Path;

use typedpg_ts::config::Config;
use typedpg_ts::project::Project;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn project(root: &Path, check: bool) -> Project {
    Project::new(
        Config::load(&root.join("typedpg.config.json")).unwrap(),
        check,
    )
    .unwrap()
}

const USERS: &str = "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);";

#[test]
fn re_exports_are_followed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "include": ["src"], "out": "src/db/index.ts" }"#,
    );
    write(&root.join("migrations/0001.sql"), USERS);
    // A barrel re-exporting the module's functions, renamed and not.
    write(
        &root.join("src/barrel.ts"),
        "export { sql as query, copyIn } from './db/index.ts';\nexport * from './more.ts';\nexport * as dbns from './db/index.ts';\n",
    );
    write(
        &root.join("src/more.ts"),
        "import { sql } from './db/index.ts';\nexport { sql as q2 };\n",
    );
    write(
        &root.join("src/a.ts"),
        "import { query, q2, dbns, copyIn } from './barrel.ts';\nimport * as b from './barrel.ts';\n\
         query('SELECT 1 AS a');\nq2('SELECT 2 AS b');\ndbns.sql('SELECT 3 AS c');\nb.query('SELECT 4 AS d');\n\
         copyIn('users (id, name)');\n",
    );
    let mut p = project(root, false);
    let r = p.sync(false);
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    let out = read(&root.join("src/db/index.ts"));
    for q in [
        "SELECT 1 AS a",
        "SELECT 2 AS b",
        "SELECT 3 AS c",
        "SELECT 4 AS d",
    ] {
        assert!(out.contains(&format!("\"{q}\": {{")), "{q}\n{out}");
    }
    assert!(out.contains("\"users (id, name)\": {\n    row: {"), "{out}");
    assert!(p.is_reexporter(&root.join("src/barrel.ts")));
    assert!(!p.is_reexporter(&root.join("src/a.ts")));
}

#[test]
fn several_databases_and_javascript_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "include": ["src"], "databases": {
              "main": { "out": "src/main.js", "migrations": "m1", "int8": "string" },
              "wh": { "out": "src/wh.cjs", "migrations": "m2" } } }"#,
    );
    write(&root.join("m1/0001.sql"), USERS);
    write(
        &root.join("m2/0001.sql"),
        "CREATE TABLE facts (k int8 NOT NULL);",
    );
    write(
        &root.join("src/a.ts"),
        "import { sql } from './main.js';\nimport { sql as wh } from './wh.cjs';\n\
         sql('SELECT count(*) AS n FROM users');\nwh('SELECT k FROM facts');\nwh('SELECT name FROM users');\n",
    );
    let mut p = project(root, false);
    let r = p.sync(false);
    // The warehouse has no users table.
    assert_eq!(r.diagnostics.len(), 1, "{:?}", r.diagnostics);
    assert!(
        r.diagnostics[0]
            .render(root)
            .starts_with("src/a.ts:5:22: error: relation \"users\" does not exist"),
        "{}",
        r.diagnostics[0].render(root)
    );
    let main_js = read(&root.join("src/main.js"));
    let main_dts = read(&root.join("src/main.d.ts"));
    assert!(main_js.starts_with("// @generated"), "{main_js}");
    assert!(
        main_js.contains("import { createCopyIn, createSql } from \"@cubos/typedpg\";"),
        "{main_js}"
    );
    assert!(!main_js.contains("interface"), "{main_js}");
    // int8 as string for this database.
    assert!(main_dts.contains("n: string;"), "{main_dts}");
    assert!(
        main_dts.contains("export declare const sql: typedpg.Sql<Queries>;"),
        "{main_dts}"
    );
    let wh = read(&root.join("src/wh.cjs"));
    assert!(
        wh.contains("const { createCopyIn, createSql } = require(\"@cubos/typedpg\");"),
        "{wh}"
    );
    assert!(wh.contains("exports.sql = createSql({"), "{wh}");
    assert!(read(&root.join("src/wh.d.cts")).contains("k: bigint;"));
    // Importing the declaration (`./main`, which resolves to main.d.ts) is
    // the same module.
    write(
        &root.join("src/b.ts"),
        "import { sql } from './main';\nsql('SELECT 5 AS e');\n",
    );
    p.rescan(&root.join("src/b.ts"));
    p.sync(false);
    assert!(read(&root.join("src/main.d.ts")).contains("\"SELECT 5 AS e\""));
}

#[test]
fn check_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "include": ["src"], "out": "src/db.ts" }"#,
    );
    write(&root.join("migrations/0001.sql"), USERS);
    write(
        &root.join("src/a.ts"),
        "import { sql } from './db';\nsql('SELECT name FROM users');\n",
    );
    // No module yet: check neither creates it nor passes.
    let mut p = project(root, true);
    let r = p.sync(true);
    assert!(!root.join("src/db.ts").exists());
    assert_eq!(r.changed, vec![root.join("src/db.ts")]);
    // Generated: check passes, then fails once a query changes.
    project(root, false).sync(false);
    let r = project(root, true).sync(true);
    assert!(r.changed.is_empty() && r.diagnostics.is_empty(), "{r:?}");
    write(
        &root.join("src/a.ts"),
        "import { sql } from './db';\nsql('SELECT id FROM users');\n",
    );
    let before = read(&root.join("src/db.ts"));
    let r = project(root, true).sync(true);
    assert_eq!(r.changed.len(), 1);
    assert_eq!(read(&root.join("src/db.ts")), before);
}

#[test]
fn embedded_migrations_and_their_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "out": "db.ts", "migrations": { "dir": "m", "table": "app.log", "lockId": 42, "embed": true } }"#,
    );
    write(&root.join("m/0001_users.sql"), USERS);
    write(&root.join("m/0001_users.down.sql"), "DROP TABLE users;");
    write(
        &root.join("m/0002_idx.sql"),
        "-- no-transaction\nCREATE INDEX CONCURRENTLY i ON users (name);",
    );
    project(root, false).sync(false);
    let out = read(&root.join("db.ts"));
    assert!(
        out.contains(
            "import { createCopyIn, createSql, embedMigrations } from \"@cubos/typedpg\";"
        ),
        "{out}"
    );
    assert!(out.contains(
        "export const migrations = embedMigrations([\n  [\"0001_users\", \"CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);\", \"DROP TABLE users;\"],\n  [\"0002_idx\", \"-- no-transaction\\nCREATE INDEX CONCURRENTLY i ON users (name);\", null],\n], { table: \"app.log\", lockId: 42, useTransaction: true, failOnDrift: true });"
    ), "{out}");
    // A migration file the runner can't read is an error where it is.
    write(&root.join("m/bad.sql"), "SELECT 1;");
    let r = project(root, false).sync(false);
    assert!(
        r.diagnostics.iter().any(|d| d
            .message
            .contains("does not follow NNNN_description.sql format")),
        "{:?}",
        r.diagnostics
    );
}

#[test]
fn a_failing_migration_is_reported_in_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("typedpg.config.json"), r#"{ "out": "db.ts" }"#);
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE t (a int);\nALTER TABLE t ADD COLUMN a int;\n",
    );
    let r = project(root, false).sync(false);
    let rendered: Vec<String> = r.diagnostics.iter().map(|d| d.render(root)).collect();
    assert_eq!(rendered.len(), 1, "{rendered:?}");
    assert!(
        rendered[0].contains("column \"a\" of relation \"t\" already exists"),
        "{}",
        rendered[0]
    );
    assert!(
        rendered[0].contains("--> migrations/0001.sql:2:"),
        "{}",
        rendered[0]
    );
}

#[test]
fn a_missing_migrations_directory_is_noted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "out": "db.ts", "migrations": "migratons" }"#,
    );
    write(
        &root.join("a.ts"),
        "import { sql } from './db';\nsql('SELECT id FROM users');\n",
    );
    let r = project(root, false).sync(false);
    assert_eq!(r.diagnostics.len(), 1);
    assert!(
        r.diagnostics[0].message.ends_with(
            "note: no migrations were loaded from 'migratons': the directory does not exist (see `migrations` in typedpg.config.json)"
        ),
        "{}",
        r.diagnostics[0].message
    );
}

#[test]
fn type_overrides_are_imported_once_per_module() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "include": ["src"], "out": "src/gen/db.ts",
             "types": { "prefs": "./src/types.ts#Prefs", "mood": "./src/types.ts#Mood", "public.tag": "my-pkg#Tag" } }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE DOMAIN prefs AS jsonb; CREATE TYPE mood AS ENUM ('a'); CREATE DOMAIN tag AS text;\n\
         CREATE TABLE t (p prefs, m mood NOT NULL, g tag);",
    );
    write(
        &root.join("src/a.ts"),
        "import { sql } from './gen/db';\nsql('SELECT p, m, g FROM t');\n",
    );
    project(root, false).sync(false);
    let out = read(&root.join("src/gen/db.ts"));
    assert!(out.contains("import type { Mood, Prefs } from \"../types.ts\";\nimport type { Tag } from \"my-pkg\";"), "{out}");
    assert!(
        out.contains("p: Prefs | null;\n      m: Mood;\n      g: Tag | null;"),
        "{out}"
    );
    // Each must fit what its PG type is read as, sorted by PG type.
    assert!(
        out.contains(
            "export type TypeMappingChecks = [\n  typedpg.Fits<Mood, string>, // public.mood\n  \
             typedpg.Fits<Prefs, unknown>, // public.prefs\n  typedpg.Fits<Tag, string>, // public.tag\n];"
        ),
        "{out}"
    );
    // An override can't take the runtime namespace's name, nor be two
    // modules' types under one name.
    write(
        &root.join("typedpg.config.json"),
        r#"{ "out": "db.ts", "types": { "x": "./a#typedpg" } }"#,
    );
    let err = Project::new(
        Config::load(&root.join("typedpg.config.json")).unwrap(),
        true,
    )
    .err()
    .unwrap();
    assert!(err.contains("`typedpg` names the runtime's types"), "{err}");
    write(
        &root.join("typedpg.config.json"),
        r#"{ "out": "db.ts", "types": { "x": "./a#T", "y": "./b#T" } }"#,
    );
    let err = Project::new(
        Config::load(&root.join("typedpg.config.json")).unwrap(),
        true,
    )
    .err()
    .unwrap();
    assert!(err.contains("two modules export a type named `T`"), "{err}");
}

#[test]
fn a_type_override_of_no_type_is_reported_in_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "out": "db.ts", "types": { "jsonpath": "./types.ts#Path", "pg_catalog.int8": "./types.ts#Id" } }"#,
    );
    write(&root.join("migrations/0001.sql"), USERS);
    let r = project(root, false).sync(false);
    let messages: Vec<String> = r.diagnostics.iter().map(|d| d.render(root)).collect();
    assert_eq!(
        messages,
        [
            "typedpg.config.json: error: `types`: type \"public.jsonpath\" does not exist (an \
          unqualified name is in `public`; a built-in type is `pg_catalog.<name>`)"
        ]
    );
    assert!(
        read(&root.join("db.ts")).contains("  typedpg.Fits<Id, bigint>, // pg_catalog.int8\n];"),
        "the other override is still checked"
    );
}
