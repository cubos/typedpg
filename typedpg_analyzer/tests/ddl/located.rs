//! `PgCatalog::apply_migration`: a failing migration's error, located in
//! the file — PG's message first, then `--> file:line:col` and the
//! offending line with a caret.

use crate::common::*;

/// Apply `init` cleanly, then `sql` as the migration `filename`, which
/// must fail; returns the rendered error.
#[track_caller]
fn located_error(init: &str, filename: &str, sql: &str) -> String {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(init).unwrap();
    match db.apply_migration(filename, sql) {
        Err(e @ DdlError::Migration { .. }) => e.to_string(),
        other => panic!("expected a located migration error, got {other:?}"),
    }
}

const POSTS: &str = "CREATE TABLE posts (id bigserial PRIMARY KEY, title text NOT NULL);";

#[test]
fn view_error_points_at_the_column_with_a_suggestion() {
    let sql = "\
-- Tags for posts.
CREATE TABLE tags (
    id bigserial PRIMARY KEY,
    label text NOT NULL
);

CREATE VIEW post_tags AS
    SELECT p.id, p.title,
           t.lable AS tag
      FROM posts p CROSS JOIN tags t;
";
    assert_eq!(
        located_error(POSTS, "migrations/0002_more.sql", sql),
        "\
column t.lable does not exist (while analyzing view 'public.post_tags')
  --> migrations/0002_more.sql:9:12
  ╭────
9 │            t.lable AS tag
  ·            ───┬───
  ·               ╰─ column does not exist
  ╰────
  help: did you mean \"label\"?"
    );
}

#[test]
fn syntax_error_points_at_its_token() {
    let sql = "CREATE TABLE a (id int);\nCREATE TABLE b (id int,, x int);\n";
    assert_eq!(
        located_error(POSTS, "0003.sql", sql),
        "\
syntax error at or near \",\"
  --> 0003.sql:2:24
  ╭────
2 │ CREATE TABLE b (id int,, x int);
  ·                        ─
  ╰────"
    );
}

#[test]
fn error_without_position_points_at_the_statement() {
    // The interpreter raises this one with no span: the caret goes to the
    // statement's first token, past the comments leading into it.
    let sql = "CREATE TABLE a (id int);\n\n/* again */ -- oops\nCREATE TABLE a (id int);\n";
    assert_eq!(
        located_error(POSTS, "0004.sql", sql),
        "\
relation \"a\" already exists
  --> 0004.sql:4:1
  ╭────
4 │ CREATE TABLE a (id int);
  · ───┬──
  ·    ╰─ in this statement
  ╰────"
    );
}

#[test]
fn apply_sql_keeps_the_plain_message() {
    // Without a file name the error is the statement's own, unchanged —
    // what the pg_sanity oracle compares.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(POSTS).unwrap();
    assert_ddl_err!(
        db.apply_sql("CREATE VIEW v AS SELECT titel FROM posts;"),
        DdlError::ViewAnalysis { .. },
        "column \"titel\" does not exist (while analyzing view 'public.v')"
    );
}

#[test]
fn function_body_errors_point_at_the_statement() {
    // A string body is parsed on its own: its locations are not offsets
    // into the migration, so the statement is what gets pointed at.
    let sql =
        "SELECT 1;\nCREATE FUNCTION f() RETURNS text LANGUAGE sql AS 'SELECT titel FROM posts';\n";
    let rendered = located_error(POSTS, "0005.sql", sql);
    assert!(
        rendered.contains("  --> 0005.sql:2:1\n"),
        "unexpected location:\n{rendered}"
    );
}
