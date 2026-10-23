//! The bits of SQL text the analyzer and the macro need outside the lexer.

/// The SQL without what follows its last statement: a trailing `;`, and
/// trailing comments.
///
/// PostgreSQL rejects a `;` inside a subquery, and a trailing `--` comment
/// would swallow the subquery's closing parenthesis — so a query that ends
/// with either cannot be embedded in one. A `;` ends the statement, so
/// everything after it is junk too.
///
/// ```
/// use typedpg_analyzer::strip_trailing_junk;
///
/// assert_eq!(strip_trailing_junk("SELECT 1;"), "SELECT 1");
/// assert_eq!(strip_trailing_junk("SELECT 1 -- a note"), "SELECT 1");
/// assert_eq!(strip_trailing_junk("SELECT ';'"), "SELECT ';'");
/// ```
pub fn strip_trailing_junk(sql: &str) -> &str {
    let bytes = sql.as_bytes();
    let mut i = 0;
    // The end of the last *code* token: what follows it is whitespace, a
    // comment, or a `;` — the statement's terminator, not part of it.
    let mut code_end = 0;
    while i < bytes.len() {
        let (step, is_code) = match bytes[i] {
            b'\'' => (skip_string(sql, i), true),
            b'"' => (skip_quoted_identifier(sql, i), true),
            b'-' if bytes.get(i + 1) == Some(&b'-') => (skip_line_comment(bytes, i), false),
            b'/' if bytes.get(i + 1) == Some(&b'*') => (skip_block_comment(bytes, i), false),
            b'$' => match dollar_quote_end(sql, i) {
                Some(end) => (end, true),
                None => (next_char(sql, i), true),
            },
            // A `;` ends the statement: it is not part of the code.
            b';' => (i + 1, false),
            c if c.is_ascii_whitespace() => (next_char(sql, i), false),
            _ => (next_char(sql, i), true),
        };
        i = step;
        if is_code {
            code_end = i;
        }
    }
    sql.get(..code_end).unwrap_or(sql).trim_end()
}

/// The index past the character at `i`.
fn next_char(sql: &str, i: usize) -> usize {
    match sql[i..].chars().next() {
        Some(c) => i + c.len_utf8(),
        None => sql.len(),
    }
}

/// The index past the string literal opened at `open`.
fn skip_string(sql: &str, open: usize) -> usize {
    let bytes = sql.as_bytes();
    // `E'…'`: a string where a backslash escapes the next character.
    let escape = open >= 1
        && matches!(bytes[open - 1], b'E' | b'e')
        && (open < 2 || !is_ident_char_byte(bytes[open - 2]));
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if escape => i += 2,
            b'\'' => {
                // `''` is an escaped quote.
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    return i + 1;
                }
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

/// The index past the quoted identifier opened at `open`.
fn skip_quoted_identifier(sql: &str, open: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut i = open + 1;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            // `""` is an escaped quote.
            if bytes.get(i + 1) == Some(&b'"') {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// The index past the line comment opened at `open`, its newline included:
/// what follows the comment is the next statement, not part of it.
fn skip_line_comment(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 2;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    if i < bytes.len() { i + 1 } else { i }
}

/// The index past the (nested) block comment opened at `open`.
fn skip_block_comment(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0;
    let mut i = open;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// The index past the dollar-quoted string opened at `open`, when `open`
/// starts one (`$$…$$` or `$tag$…$tag$`).
fn dollar_quote_end(sql: &str, open: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut i = open + 1;
    if i < bytes.len() && is_ident_start_byte(bytes[i]) {
        while i < bytes.len() && is_ident_char_byte(bytes[i]) {
            i += 1;
        }
    }
    if bytes.get(i) != Some(&b'$') {
        return None;
    }
    let delimiter = &bytes[open..=i];
    let body = i + 1;
    let close = bytes[body..]
        .windows(delimiter.len())
        .position(|w| w == delimiter)?;
    Some(body + close + delimiter.len())
}

/// PG's identifier characters, in bytes (`scan.l`'s `ident_start` /
/// `ident_cont`): a letter — ASCII or any high-bit one — or an underscore,
/// plus a digit for the continuation.
fn is_ident_start_byte(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_char_byte(b: u8) -> bool {
    is_ident_start_byte(b) || b.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::strip_trailing_junk;

    #[test]
    fn a_trailing_semicolon_is_dropped() {
        assert_eq!(strip_trailing_junk("SELECT 1;"), "SELECT 1");
        assert_eq!(strip_trailing_junk("SELECT 1 ;"), "SELECT 1");
        assert_eq!(strip_trailing_junk("SELECT 1;  \n"), "SELECT 1");
    }

    #[test]
    fn trailing_comments_are_dropped() {
        assert_eq!(strip_trailing_junk("SELECT 1 -- a note"), "SELECT 1");
        assert_eq!(strip_trailing_junk("SELECT 1 -- a note\n"), "SELECT 1");
        assert_eq!(strip_trailing_junk("SELECT 1; -- a note"), "SELECT 1");
        assert_eq!(strip_trailing_junk("SELECT 1 /* a note */"), "SELECT 1");
        assert_eq!(
            strip_trailing_junk("SELECT 1 /* a /* nested */ note */"),
            "SELECT 1"
        );
        assert_eq!(
            strip_trailing_junk("SELECT 1 /* a note */ -- another"),
            "SELECT 1"
        );
    }

    #[test]
    fn a_semicolon_inside_a_literal_is_kept() {
        assert_eq!(strip_trailing_junk("SELECT ';'"), "SELECT ';'");
        assert_eq!(strip_trailing_junk("SELECT 'a; b'"), "SELECT 'a; b'");
        assert_eq!(strip_trailing_junk("SELECT \";\";"), "SELECT \";\"");
        assert_eq!(strip_trailing_junk("SELECT $$a;$$"), "SELECT $$a;$$");
        assert_eq!(
            strip_trailing_junk("SELECT $tag$a;$tag$"),
            "SELECT $tag$a;$tag$"
        );
        assert_eq!(strip_trailing_junk("SELECT E'a\\';b'"), "SELECT E'a\\';b'");
        assert_eq!(strip_trailing_junk("SELECT '--'"), "SELECT '--'");
        assert_eq!(strip_trailing_junk("SELECT '/*'"), "SELECT '/*'");
    }

    /// A `;` is the statement's terminator, not part of it: what follows
    /// it — a comment, another `;` — is junk too.
    #[test]
    fn a_semicolon_ends_the_statement() {
        assert_eq!(strip_trailing_junk("SELECT 1 -- a note\n;"), "SELECT 1");
        assert_eq!(strip_trailing_junk("SELECT 1; -- a note\n;"), "SELECT 1");
        assert_eq!(strip_trailing_junk("SELECT 1;;"), "SELECT 1");
        assert_eq!(strip_trailing_junk("SELECT 1 /* a note */;"), "SELECT 1");
        assert_eq!(
            strip_trailing_junk("SELECT 1; SELECT 2"),
            "SELECT 1; SELECT 2"
        );
    }

    #[test]
    fn a_query_without_junk_is_untouched() {
        assert_eq!(strip_trailing_junk("SELECT 1"), "SELECT 1");
        assert_eq!(strip_trailing_junk("SELECT 1 "), "SELECT 1");
        assert_eq!(strip_trailing_junk(""), "");
    }
}
