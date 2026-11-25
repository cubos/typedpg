//! The statement `sql!` runs is its literal's value, as Rust defines it.

mod common;

use typedpg::sql;

/// A line continuation (`\` at the end of a line) inside a SQL string
/// drops the line break and the next line's indentation, as it does in any
/// Rust string: the value is `'ab'`.
#[tokio::test]
async fn a_line_continuation_inside_a_sql_string_is_rusts() {
    let pool = common::setup().await;
    let row = sql!(
        &pool,
        "SELECT 'a\
                 b' AS x"
    )
    .fetch_one()
    .await
    .unwrap();
    assert_eq!(row.x, "ab");
}

/// In a `--` comment, a continuation keeps what follows in the comment:
/// the query has one column (`fetch_value` exists only then).
#[tokio::test]
async fn a_line_continuation_inside_a_comment_is_rusts() {
    let pool = common::setup().await;
    let x: i32 = sql!(
        &pool,
        "SELECT 1 AS x -- the rest of this line is a comment \
                , 2 AS y"
    )
    .fetch_value()
    .await
    .unwrap();
    assert_eq!(x, 1);
}
