//! PG's error position as psql shows it: a line of the migration and a caret.
//!
//! `tokio-postgres` reports a position as a character index into the text
//! that was sent; outside a transaction that is one statement, so the
//! runner adds the statement's byte offset to report the file's line.
//! Shared with `typedpg_wasm`, so both runners point at the same caret.

/// PG's error position `position` — a 1-based character index into
/// `sql[base..]`, the text that was sent — as psql shows it: `LINE n:` and
/// the line, then a caret under the column. `n` counts lines of the whole
/// `sql`.
pub fn locate_position(sql: &str, base: usize, position: u32) -> String {
    let sent = sql.get(base..).unwrap_or_default();
    let chars = usize::try_from(position).unwrap_or(0).saturating_sub(1);
    let at = base
        + sent
            .char_indices()
            .nth(chars)
            .map_or(sent.len(), |(i, _)| i);
    let line_start = sql[..at].rfind('\n').map_or(0, |i| i + 1);
    let line_end = sql[at..].find('\n').map_or(sql.len(), |i| at + i);
    let line_no = sql[..at].matches('\n').count() + 1;
    let column = sql[line_start..at].chars().count();
    let prefix = format!("LINE {line_no}: ");
    format!(
        "\n{prefix}{}\n{}^",
        &sql[line_start..line_end],
        " ".repeat(prefix.len() + column)
    )
}

#[cfg(test)]
mod tests {
    use super::locate_position;

    #[test]
    fn positions_are_lines_and_columns_of_the_migration() {
        let sql = "CREATE TABLE a (id int);\nCREATE TABL b (id int);\n";
        // The whole migration sent: PG's position 33 is `TABL` on line 2.
        assert_eq!(
            locate_position(sql, 0, 33),
            "\nLINE 2: CREATE TABL b (id int);\n               ^"
        );
        // One statement sent (non-transactional run): the position is
        // relative to it, the line still the file's.
        let second = sql.find("CREATE TABL b").unwrap();
        assert_eq!(
            locate_position(sql, second, 8),
            "\nLINE 2: CREATE TABL b (id int);\n               ^"
        );
        // PG counts characters, not bytes.
        assert_eq!(
            locate_position("SELECT 'é' x y", 0, 14),
            "\nLINE 1: SELECT 'é' x y\n                     ^"
        );
    }
}
