//! The incremental [`Project`] behind `--watch`: what each change makes it
//! redo.

use std::fs;
use std::path::Path;

use typedpg_ts::config::Config;
use typedpg_ts::project::Project;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn only_what_changed_is_redone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("typedpg.config.json"),
        r#"{ "include": ["src"], "out": "src/db.ts" }"#,
    );
    write(
        &root.join("migrations/0001.sql"),
        "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);",
    );
    let a = root.join("src/a.ts");
    let b = root.join("src/b.ts");
    write(
        &a,
        "import { sql } from './db';\nsql('SELECT id FROM users');\n",
    );
    write(
        &b,
        "import { sql } from './db';\nsql('SELECT name FROM users');\n",
    );

    let mut project = Project::new(
        Config::load(&root.join("typedpg.config.json")).unwrap(),
        false,
    )
    .unwrap();
    let r = project.sync(false);
    assert_eq!(
        (r.analyzed, r.changed.len(), r.diagnostics.len()),
        (2, 1, 0)
    );
    let out = fs::read_to_string(root.join("src/db.ts")).unwrap();
    assert!(out.contains("\"SELECT id FROM users\": {"), "{out}");

    // Nothing changed: nothing analyzed, nothing written.
    let r = project.sync(false);
    assert_eq!((r.analyzed, r.changed.len()), (0, 0));

    // A new query in one file: only it is analyzed.
    write(
        &a,
        "import { sql } from './db';\nsql('SELECT id FROM users');\nsql('SELECT id, name FROM users');\n",
    );
    project.rescan(&a);
    let r = project.sync(false);
    assert_eq!((r.analyzed, r.changed.len()), (1, 1));

    // A file that stops parsing keeps its queries meanwhile.
    write(
        &a,
        "import { sql } from './db';\nsql('SELECT id FROM users'\n",
    );
    project.rescan(&a);
    let r = project.sync(false);
    assert_eq!((r.analyzed, r.changed.len()), (0, 0));

    // A removed query leaves the module.
    write(&b, "export {};\n");
    project.rescan(&b);
    let r = project.sync(false);
    assert_eq!((r.analyzed, r.changed.len()), (0, 1));
    let out = fs::read_to_string(root.join("src/db.ts")).unwrap();
    assert!(!out.contains("SELECT name FROM users"), "{out}");

    // A migration: every query of the database is analyzed again.
    write(
        &root.join("migrations/0002.sql"),
        "ALTER TABLE users ALTER COLUMN name DROP NOT NULL;",
    );
    for db in project.migration_dbs(&root.join("migrations/0002.sql")) {
        project.reload_migrations(db);
    }
    let r = project.sync(false);
    assert_eq!((r.analyzed, r.changed.len()), (2, 1));
    let out = fs::read_to_string(root.join("src/db.ts")).unwrap();
    assert!(out.contains("name: string | null;"), "{out}");

    // An error is reported where the query is, and lands in the module.
    write(
        &b,
        "import { sql as q } from './db.js';\n\nq(`SELECT nope FROM users`);\n",
    );
    project.rescan(&b);
    let r = project.sync(false);
    assert_eq!(r.analyzed, 1);
    assert_eq!(r.diagnostics.len(), 1);
    assert!(
        r.diagnostics[0]
            .render(root)
            .starts_with("src/b.ts:3:11: error: column \"nope\" does not exist"),
        "{}",
        r.diagnostics[0].render(root)
    );
    let out = fs::read_to_string(root.join("src/db.ts")).unwrap();
    assert!(
        out.contains(r#""SELECT nope FROM users": { error: "column \"nope\" does not exist" };"#),
        "{out}"
    );
}
