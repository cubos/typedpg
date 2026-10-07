//! `analyze_located`: an error with the offset in the query it points at.

use typedpg_analyzer::PgCatalog;

fn catalog() -> PgCatalog {
    let mut c = PgCatalog::new().unwrap();
    c.apply_migration("0001.sql", "CREATE TABLE users (id int4, name text);")
        .unwrap();
    c
}

#[test]
fn an_error_has_the_offset_it_points_at() {
    let c = catalog();
    let sql = "SELECT id,\n  nmae FROM users";
    let e = c.analyze_located(sql).unwrap_err();
    assert!(
        e.error
            .to_string()
            .starts_with("column \"nmae\" does not exist")
    );
    assert_eq!(e.offset, Some(sql.find("nmae").unwrap()));

    // Through a named parameter, which the lexer rewrites to `$1`: the
    // offset is in the text as written.
    let sql = "SELECT id FROM users WHERE id = $id AND nope = 1";
    let e = c.analyze_located(sql).unwrap_err();
    assert_eq!(e.offset, Some(sql.find("nope").unwrap()));
}

#[test]
fn syntax_errors_are_located() {
    let c = catalog();
    let sql = "SELECT id FROM users WHERE";
    let e = c.analyze_located(sql).unwrap_err();
    assert!(
        e.error
            .to_string()
            .starts_with("syntax error at end of input"),
        "{}",
        e.error
    );
    let sql = "SELECT id FROM users WHER id = 1";
    let e = c.analyze_located(sql).unwrap_err();
    assert_eq!(e.offset, Some(sql.find("id = 1").unwrap()), "{}", e.error);
}

#[test]
fn copy_in_targets_are_located() {
    let c = catalog();
    let target = "users (id,, name)";
    let e = c.analyze_copy_in_located(target).unwrap_err();
    assert!(
        e.error
            .to_string()
            .starts_with("syntax error at or near \",\""),
        "{}",
        e.error
    );
    assert_eq!(e.offset, Some(10));
    // A column check has no location: the error is the whole target's.
    let e = c.analyze_copy_in_located("users (id, nmae)").unwrap_err();
    assert!(e.error.to_string().contains("\"nmae\""), "{}", e.error);
}
