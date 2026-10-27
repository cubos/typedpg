//! The normalized migration text drift detection compares.
//!
//! Drift must notice a change to a migration's statements, and nothing
//! else: comments, whitespace and the case of unquoted words do not count.
//! The scan follows psql's lexer, as `split.rs` does, for the constructs
//! where those characters are content — strings, quoted identifiers and
//! dollar-quoted bodies — which are copied verbatim.

use super::source::has_no_transaction_marker;
use super::split::{is_ident_cont, is_ident_start, skip_block_comment, skip_quoted};

/// Which normalization the stored hashes use. A row's `hash_version` names
/// the algorithm that computed its `hash`; `run` re-hashes rows of another
/// version from their `sql_source`.
pub(crate) const HASH_VERSION: i32 = 1;

/// The MD5 hex digest of [`normalize`]'s UTF-8 bytes.
pub(crate) fn hash(sql: &str) -> String {
    format!("{:x}", md5::compute(normalize(sql).as_bytes()))
}

/// `sql` normalized so that comments, whitespace and the case of unquoted
/// words do not count as drift, while any change to the statements does.
/// Unquoted ASCII letters are lowercased, as PostgreSQL folds them; strings,
/// quoted identifiers and dollar-quoted bodies are content and copied
/// verbatim.
///
/// A maximal run of whitespace and/or comments becomes one ASCII space only
/// when the character just before it and the character just after it would
/// fuse into a single token if they touched — both are in the same class:
///
/// - word class: ASCII letters, digits, `_`, `$`, `.`, any non-ASCII
///   character, and the `'` or `"` that opens a literal. A whole verbatim
///   literal counts as a word on both of its ends;
/// - operator class: PostgreSQL's operator characters — `+`, `-`, `*`, `/`,
///   `<`, `>`, `=`, `~`, `!`, `@`, `#`, `%`, `^`, `&`, `|`, `` ` `` and `?`;
/// - colon class: `:` next to `:` (`::`).
///
/// Any other pair — brackets, commas, semicolons, or a word next to an
/// operator or punctuation — drops the run entirely, comment or not: `t (a`,
/// `t(a)` and `t\n(\n  a` are the same text, as are `a - b` and `a-b`,
/// while `a b`, `a/**/b`, `- -1` and `: :` keep the boundary they spell.
///
/// Whitespace only matters where removing it would fuse two tokens; dropping
/// it everywhere else is what makes reformatting — indentation, line breaks
/// around punctuation — not drift. No two texts PostgreSQL lexes differently
/// normalize the same, except one accepted edge: a line break between two
/// string literals (`'a'\n'b'`) makes PostgreSQL concatenate them, while a
/// plain space (`'a' 'b'`) leaves two literals; both normalize alike.
///
/// The `-- no-transaction` marker decides how the migration runs, so it is
/// part of the normalized text (the comment scan drops it otherwise).
pub(crate) fn normalize(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut last: Option<Class> = None;
    let mut pending_run = false;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            // Comments and whitespace form one maximal run: whether it
            // leaves a space is decided when the next token starts.
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                pending_run = true;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(bytes, i);
                pending_run = true;
            }
            b if b.is_ascii_whitespace() => {
                pending_run = true;
                i += 1;
            }
            b'\'' => {
                // `E'…'`, where a backslash escapes the next character: the
                // `E` only prefixes the string when it is a word of its own.
                let escapes = i > 0
                    && matches!(bytes[i - 1], b'e' | b'E')
                    && (i < 2 || !is_ident_cont(bytes[i - 2]));
                let end = skip_quoted(bytes, i, b'\'', escapes);
                copy_verbatim(&mut out, &mut last, &mut pending_run, &sql[i..end]);
                i = end;
            }
            b'"' => {
                let end = skip_quoted(bytes, i, b'"', false);
                copy_verbatim(&mut out, &mut last, &mut pending_run, &sql[i..end]);
                i = end;
            }
            b'$' => match dollar_quote_span(bytes, i) {
                Some(end) => {
                    copy_verbatim(&mut out, &mut last, &mut pending_run, &sql[i..end]);
                    i = end;
                }
                None => {
                    // A `$1` parameter, or any other `$`: ordinary text.
                    copy_ordinary(&mut out, &mut last, &mut pending_run, b);
                    i += 1;
                }
            },
            // A word, scanned whole: `$` inside an identifier starts no
            // dollar quote, and the `E` of `E'…'` is a word of its own.
            _ if is_ident_start(b) => {
                let start = i;
                while i < bytes.len() && is_ident_cont(bytes[i]) {
                    i += 1;
                }
                copy_word(&mut out, &mut last, &mut pending_run, &sql[start..i]);
            }
            _ => {
                copy_ordinary(&mut out, &mut last, &mut pending_run, b);
                i += 1;
            }
        }
    }

    if has_no_transaction_marker(sql) {
        format!("no-transaction\n{out}")
    } else {
        out
    }
}

/// What a character would fuse with if a run between them disappeared:
/// touching two characters of one class makes one token, of different
/// classes (or of [`Class::Other`]) stays two.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Letters, digits, `_`, `$`, `.`, non-ASCII characters, and the quotes
    /// that open a literal. A whole literal is a word at both of its ends.
    Word,
    /// A PostgreSQL operator character.
    Operator,
    /// `:`, which only fuses with another `:`.
    Colon,
    /// Everything else: brackets, commas, semicolons, …
    Other,
}

impl Class {
    /// The class of the single byte `b`.
    fn of(b: u8) -> Self {
        if is_ident_start(b) || b.is_ascii_digit() || matches!(b, b'$' | b'.' | b'\'' | b'"') {
            Class::Word
        } else if b"+-*/<>=~!@#%^&|`?".contains(&b) {
            Class::Operator
        } else if b == b':' {
            Class::Colon
        } else {
            Class::Other
        }
    }
}

/// Start a token of `class`: write the one space a pending run calls for
/// exactly when it would otherwise fuse this token with the previous one.
/// A leading run (no previous token) or one next to [`Class::Other`] writes
/// nothing.
fn separate(out: &mut String, last: &mut Option<Class>, pending_run: &mut bool, class: Class) {
    if *pending_run {
        if *last == Some(class) && class != Class::Other {
            out.push(' ');
        }
        *pending_run = false;
    }
    *last = Some(class);
}

/// Append `text` — a string, quoted identifier or dollar-quoted body —
/// unchanged: a literal is a word on both of its ends.
fn copy_verbatim(out: &mut String, last: &mut Option<Class>, pending_run: &mut bool, text: &str) {
    separate(out, last, pending_run, Class::Word);
    out.push_str(text);
}

/// Append one ordinary character, folding ASCII letters only: PostgreSQL
/// folds unquoted identifiers the same way.
fn copy_ordinary(out: &mut String, last: &mut Option<Class>, pending_run: &mut bool, b: u8) {
    separate(out, last, pending_run, Class::of(b));
    out.push(char::from(b).to_ascii_lowercase());
}

/// Append an unquoted word, folding ASCII letters only: PostgreSQL folds
/// unquoted identifiers the same way, and leaves every other character as
/// the bytes it was (a multi-byte character must not be re-encoded).
fn copy_word(out: &mut String, last: &mut Option<Class>, pending_run: &mut bool, word: &str) {
    separate(out, last, pending_run, Class::Word);
    for c in word.chars() {
        out.push(c.to_ascii_lowercase());
    }
}

/// The index past a dollar-quoted body opened at `open`: `None` for a `$`
/// that opens none (`$1`). An unterminated body copies the rest verbatim.
fn dollar_quote_span(bytes: &[u8], open: usize) -> Option<usize> {
    let mut i = open + 1;
    if i < bytes.len() && is_ident_start(bytes[i]) {
        while i < bytes.len() && (is_ident_start(bytes[i]) || bytes[i].is_ascii_digit()) {
            i += 1;
        }
    }
    if bytes.get(i) != Some(&b'$') {
        return None;
    }
    let delimiter = &bytes[open..=i];
    let body = i + 1;
    Some(
        match bytes[body..]
            .windows(delimiter.len())
            .position(|w| w == delimiter)
        {
            Some(close) => body + close + delimiter.len(),
            None => bytes.len(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{hash, normalize};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Case {
        a: String,
        b: String,
        same: bool,
        why: String,
    }

    /// The cases are shared with the TypeScript runtime's own test: the two
    /// runners must agree on what drift is, or one database would look
    /// drifted to one CLI and not the other.
    #[test]
    fn normalization_cases_are_the_shared_ones() {
        let json = include_str!("../../tests/fixtures/normalize.json");
        let cases: Vec<Case> = serde_json::from_str(json).expect("parse normalize.json");
        assert!(cases.len() >= 25, "only {} cases", cases.len());
        for case in cases {
            assert_eq!(
                hash(&case.a) == hash(&case.b),
                case.same,
                "{}:\n  a = {:?}\n  b = {:?}",
                case.why,
                case.a,
                case.b
            );
        }
    }

    /// Both runners hash the normalized text's UTF-8 bytes: a non-ASCII
    /// character in an unquoted word must pass through unchanged, not be
    /// re-encoded byte by byte. The hash is pinned in the TypeScript test
    /// too, so the two cannot drift apart.
    #[test]
    fn non_ascii_words_are_kept() {
        assert_eq!(normalize("SELECT Café FROM t;"), "select café from t;");
        assert_eq!(
            hash("SELECT Café FROM t;"),
            "5a3c8a972c93614feb3618f2e3ce47b1"
        );
    }
}
