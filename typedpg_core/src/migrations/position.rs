//! PG's error position as psql shows it: a line of the migration and a caret.
//!
//! `tokio-postgres` reports a position as a character index into the text
//! that was sent; outside a transaction that is one statement, so the
//! runner adds the statement's byte offset to report the file's line.
//! Shared with `typedpg_wasm`, so both runners point at the same caret.

use crate::display_width::pg_wcwidth;

/// PG's error position `position` — a 1-based character index into
/// `sql[base..]`, the text that was sent — as psql shows it: `LINE n:` and
/// the line, then a caret under the column. `n` counts lines of the whole
/// `sql`. Empty when the position is past the text, as psql prints nothing
/// then.
///
/// This is libpq's `reportErrorPosition` (`fe-protocol3.c`), for UTF-8: a
/// CR or an LF ends a line, a CRLF being one line break; a tab shows as one
/// space; a column is as wide as `pg_encoding_dsplen` says (two for a wide
/// character, one for any other, control and non-spacing characters
/// included); and a line wider than 60 columns is cut to 60 around the
/// caret, `...` marking each cut end.
pub fn locate_position(sql: &str, base: usize, position: u32) -> String {
    /// libpq's `DISPLAY_SIZE` and `MIN_RIGHT_CUT`, in screen columns.
    const DISPLAY_SIZE: usize = 60;
    const MIN_RIGHT_CUT: usize = 10;

    let Some(loc) = usize::try_from(position)
        .ok()
        .and_then(|position| position.checked_sub(1))
    else {
        return String::new();
    };
    let base = (0..=base.min(sql.len()))
        .rev()
        .find(|&i| sql.is_char_boundary(i))
        .unwrap_or(0);
    let loc = sql[..base].chars().count() + loc;

    // Per character up to the end of the error's line: its byte offset and
    // its first screen column (`qidx`, `scridx`).
    let mut qidx = Vec::new();
    let mut scridx = Vec::new();
    let mut scroffset = 0;
    let mut loc_line = 1;
    let mut ibeg = 0;
    let mut iend = None;
    let mut previous = None;
    for (cno, (offset, ch)) in sql.char_indices().enumerate() {
        qidx.push(offset);
        scridx.push(scroffset);
        if ch == '\r' || ch == '\n' {
            if cno < loc {
                if ch == '\r' || previous != Some('\r') {
                    loc_line += 1;
                }
                ibeg = cno + 1;
            } else {
                iend = Some(cno);
                break;
            }
        }
        // A tab is shown as a space, one column wide.
        let shown = if ch == '\t' { ' ' } else { ch };
        scroffset += usize::try_from(pg_wcwidth(shown)).unwrap_or(0).max(1);
        previous = Some(ch);
    }
    let mut iend = match iend {
        Some(iend) => iend,
        None => {
            qidx.push(sql.len());
            scridx.push(scroffset);
            qidx.len() - 1
        }
    };
    if loc > iend {
        return String::new();
    }

    let mut beg_trunc = false;
    let mut end_trunc = false;
    if scridx[iend] - scridx[ibeg] > DISPLAY_SIZE {
        if scridx[ibeg] + DISPLAY_SIZE >= scridx[loc] + MIN_RIGHT_CUT {
            while scridx[iend] - scridx[ibeg] > DISPLAY_SIZE {
                iend -= 1;
            }
            end_trunc = true;
        } else {
            while scridx[loc] + MIN_RIGHT_CUT < scridx[iend] {
                iend -= 1;
                end_trunc = true;
            }
            while scridx[iend] - scridx[ibeg] > DISPLAY_SIZE {
                ibeg += 1;
                beg_trunc = true;
            }
        }
    }

    let mut prefix = format!("LINE {loc_line}: ");
    if beg_trunc {
        prefix.push_str("...");
    }
    let mut line = sql[qidx[ibeg]..qidx[iend]].replace('\t', " ");
    if end_trunc {
        line.push_str("...");
    }
    format!(
        "\n{prefix}{line}\n{}^",
        " ".repeat(prefix.len() + scridx[loc] - scridx[ibeg])
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

    // What psql (libpq's `reportErrorPosition`) prints for the same errors:
    // a file saved with CRLF line breaks shows no stray CR, a lone CR ends
    // a line too, and a tab shows as a space so the caret stays under it.
    #[test]
    fn line_breaks_and_tabs_are_shown_as_psql_does() {
        let crlf = "CREATE TABLE a (id int);\r\nCREATE TABL b (id int);\r\n";
        assert_eq!(
            locate_position(crlf, 0, 34),
            "\nLINE 2: CREATE TABL b (id int);\n               ^"
        );
        let cr = "CREATE TABLE a (id int);\rCREATE TABL b (id int);\r";
        assert_eq!(
            locate_position(cr, 0, 33),
            "\nLINE 2: CREATE TABL b (id int);\n               ^"
        );
        let tab = "CREATE TABLE a (\n\tid int,\n\tx nosuchtype\n);";
        assert_eq!(
            locate_position(tab, 0, 30),
            "\nLINE 3:  x nosuchtype\n           ^"
        );
    }

    /// What psql 19 prints (`psql -f` of each text, in a UTF-8 database):
    /// a wide character (CJK, Hangul, emoji) takes two columns, a combining
    /// mark and a control character one each (libpq counts a character
    /// `pg_encoding_dsplen` gives no width as one), and a line wider than 60
    /// columns is cut around the caret with `...`.
    #[test]
    fn columns_and_cuts_are_psqls() {
        let cases: &[(&str, u32, &str)] = &[
            // cjk
            (
                "SELECT '表表表' x y;",
                16,
                "\nLINE 1: SELECT '表表表' x y;\n                          ^",
            ),
            // combining
            (
                "SELECT 'e\u{301}e\u{301}' x y;",
                17,
                "\nLINE 1: SELECT 'e\u{301}e\u{301}' x y;\n                        ^",
            ),
            // control
            (
                "SELECT '\u{1}\u{2}' x y;",
                15,
                "\nLINE 1: SELECT '\u{1}\u{2}' x y;\n                      ^",
            ),
            // emoji
            (
                "SELECT '😀🚀' x y;",
                15,
                "\nLINE 1: SELECT '😀🚀' x y;\n                        ^",
            ),
            // hangul_line2
            (
                "SELECT 1,\n  '한국어' x y;",
                21,
                "\nLINE 2:   '한국어' x y;\n                     ^",
            ),
            // long_end
            (
                "SELECT 'abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij' AS c1, 'more text here' x y;",
                97,
                "\nLINE 1: ...abcdefghijabcdefghijabcdefghij' AS c1, 'more text here' x y;\n                                                                     ^",
            ),
            // long_middle
            (
                "SELECT 'abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij' x y, 'abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij';",
                73,
                "\nLINE 1: ...efghijabcdefghijabcdefghijabcdefghijabcdefghij' x y, 'abcdef...\n                                                             ^",
            ),
            // long_start
            (
                "SELECT x y, 'abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij';",
                8,
                "\nLINE 1: SELECT x y, 'abcdefghijabcdefghijabcdefghijabcdefghijabcdefg...\n               ^",
            ),
            // long_wide
            (
                "SELECT '表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表' x y;",
                53,
                "\nLINE 1: ...表表表表表表表表表表表表表表表表表表表表表表表表表表表' x y;\n                                                                     ^",
            ),
            // long_wide_middle
            (
                "SELECT '表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表' x y, '表表表表表表表表表表表表表表表表表表表表表表表表表表表表表表';",
                43,
                "\nLINE 1: ...表表表表表表表表表表表表表表表表表表表表表表表' x y, '表表表...\n                                                             ^",
            ),
            // tab_wide
            (
                "SELECT\t'表'\tx y;",
                14,
                "\nLINE 1: SELECT '表' x y;\n                      ^",
            ),
        ];
        for &(sql, position, expected) in cases {
            assert_eq!(locate_position(sql, 0, position), expected, "{sql:?}");
        }
    }

    /// psql prints nothing for a position past the text, or 0.
    #[test]
    fn a_position_past_the_text_shows_nothing() {
        assert_eq!(locate_position("SELECT", 0, 8), "");
        assert_eq!(locate_position("SELECT", 0, 0), "");
        // One past the last character is the end of the input (psql:
        // `SELECT (` is a syntax error at end of input, position 9).
        assert_eq!(
            locate_position("SELECT (", 0, 9),
            "\nLINE 1: SELECT (\n                ^"
        );
        // A CRLF before a wide character: one line break, two columns.
        assert_eq!(
            locate_position("SELECT\r\n表 x y", 0, 13),
            "\nLINE 2: 表 x y\n             ^"
        );
    }
}
