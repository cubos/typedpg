//! The normalized migration text drift detection compares.
//!
//! Drift must notice a change to a migration's statements, and nothing
//! else: comments, whitespace and the case of unquoted words do not count.
//! The scan follows psql's lexer, as [`super::split`] does, for the
//! constructs where those characters are content — strings, quoted
//! identifiers and dollar-quoted bodies — which are copied verbatim.
//!
//! Shared with `typedpg_wasm`, which the npm runtime calls: a database
//! looks drifted to one runner exactly when it does to the other.

use std::borrow::Cow;

use super::split::{
    is_ident_cont, is_ident_start, is_space, skip_block_comment, skip_line_comment, skip_quoted,
};
use crate::migration_marker::has_no_transaction_marker;

/// Which normalization the stored hashes use. A row's `hash_version` names
/// the algorithm that computed its `hash`; `run` re-hashes rows of another
/// version from their `sql_source`, and a row of an older version without
/// one is compared with that version's algorithm ([`hash_with_version`]).
///
/// - 1: typedpg 0.6 to 0.8. Line breaks were content inside literals (a
///   CRLF checkout of an LF file drifted), and `\v` was not whitespace.
/// - 2: every line break is one LF, and whitespace is PostgreSQL's.
pub const HASH_VERSION: i32 = 2;

/// The MD5 hex digest of [`normalize`]'s UTF-8 bytes.
pub fn hash(sql: &str) -> String {
    digest(&normalize(sql))
}

/// The hash of `sql` under the algorithm `version` names, as rows of that
/// version store it: `None` for a version this release does not know.
pub fn hash_with_version(sql: &str, version: i32) -> Option<String> {
    match version {
        1 => Some(digest(&normalize_as(sql, Version::V1))),
        HASH_VERSION => Some(hash(sql)),
        _ => None,
    }
}

fn digest(normalized: &str) -> String {
    format!("{:x}", md5::compute(normalized.as_bytes()))
}

/// The hashing algorithms [`hash_with_version`] knows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Version {
    /// `hash_version` 1: line breaks kept as written, `\v` not whitespace.
    V1,
    /// The current one, [`HASH_VERSION`].
    Current,
}

/// `sql` normalized so that comments, whitespace and the case of unquoted
/// words do not count as drift, while any change to the statements does.
/// Unquoted ASCII letters are lowercased, as PostgreSQL folds them; strings,
/// quoted identifiers and dollar-quoted bodies are content and copied
/// verbatim.
///
/// A CRLF or a lone CR is the same line break as an LF everywhere in the
/// file, inside literals and dollar-quoted bodies too: checking a migration
/// out with other line endings (git's `autocrlf`) is no edit by its author.
/// The statements run are the file as it is; only the comparison ignores
/// the difference. Whitespace is what PostgreSQL's lexer skips: space,
/// `\t`, `\n`, `\r`, `\v` and `\f`.
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
/// One sequence keeps a space no run spelled: the word `u`, the operator
/// `&` and a quoted literal, which touching would be the Unicode-escape
/// literal `u&'…'` (`u&"…"`), become `u& '…'` however they were spaced.
///
/// The `-- no-transaction` marker decides how the migration runs, so it is
/// part of the normalized text (the comment scan drops it otherwise).
pub fn normalize(sql: &str) -> String {
    normalize_as(sql, Version::Current)
}

fn normalize_as(sql: &str, version: Version) -> String {
    let unified;
    let (sql, is_space): (&str, fn(u8) -> bool) = match version {
        Version::V1 => (sql, |b| b.is_ascii_whitespace()),
        Version::Current => {
            unified = unify_line_breaks(sql);
            (&unified, is_space)
        }
    };
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut last: Option<Class> = None;
    let mut pending_run = false;
    // The tokens just written, as far as a Unicode-escape prefix goes.
    let mut prefix = UnicodePrefix::None;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            // Comments and whitespace form one maximal run: whether it
            // leaves a space is decided when the next token starts.
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = skip_line_comment(bytes, i);
                pending_run = true;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(bytes, i);
                pending_run = true;
            }
            b if is_space(b) => {
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
                keep_apart_from_u_amp(&mut out, &mut last, &mut pending_run, prefix);
                copy_verbatim(&mut out, &mut last, &mut pending_run, &sql[i..end]);
                prefix = UnicodePrefix::None;
                i = end;
            }
            b'"' => {
                let end = skip_quoted(bytes, i, b'"', false);
                keep_apart_from_u_amp(&mut out, &mut last, &mut pending_run, prefix);
                copy_verbatim(&mut out, &mut last, &mut pending_run, &sql[i..end]);
                prefix = UnicodePrefix::None;
                i = end;
            }
            b'$' => match dollar_quote_span(bytes, i) {
                Some(end) => {
                    copy_verbatim(&mut out, &mut last, &mut pending_run, &sql[i..end]);
                    prefix = UnicodePrefix::None;
                    i = end;
                }
                None => {
                    // A `$1` parameter, or any other `$`: ordinary text.
                    copy_ordinary(&mut out, &mut last, &mut pending_run, b);
                    prefix = UnicodePrefix::None;
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
                let word = &sql[start..i];
                let is_u = word.eq_ignore_ascii_case("u");
                match bytes.get(i..i + 2) {
                    // scan.l's `xusstart`/`xuistart`: `U&'…'` and `U&"…"`,
                    // the `U&` touching the quote, are one Unicode-escape
                    // literal (no backslash escapes, the quote doubled).
                    Some([b'&', quote @ (b'\'' | b'"')]) if is_u => {
                        let end = skip_quoted(bytes, i + 1, *quote, false);
                        copy_word(&mut out, &mut last, &mut pending_run, word);
                        out.push_str(&sql[i..end]);
                        prefix = UnicodePrefix::None;
                        i = end;
                    }
                    _ => {
                        copy_word(&mut out, &mut last, &mut pending_run, word);
                        prefix = if is_u {
                            UnicodePrefix::U
                        } else {
                            UnicodePrefix::None
                        };
                    }
                }
            }
            _ => {
                copy_ordinary(&mut out, &mut last, &mut pending_run, b);
                prefix = if prefix == UnicodePrefix::U && b == b'&' {
                    UnicodePrefix::UAmp
                } else {
                    UnicodePrefix::None
                };
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

/// `sql` with every CRLF and lone CR written as an LF.
fn unify_line_breaks(sql: &str) -> Cow<'_, str> {
    if sql.contains('\r') {
        Cow::Owned(sql.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(sql)
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

/// The word `u` and the operator `&` just written, as separate tokens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnicodePrefix {
    None,
    /// The last token is the word `u` (or `U`).
    U,
    /// The last two tokens are the word `u` and a lone `&`.
    UAmp,
}

/// Before a quoted literal: if the separate tokens `u` and `&` precede it,
/// write a space, so the text cannot read as the Unicode-escape literal
/// `u&'…'` (which is written without one). `u & 'a'`, `u &'a'` and
/// `u& 'a'` all lex as `u`, `&`, `'a'`, and normalize to `u& 'a'`.
fn keep_apart_from_u_amp(
    out: &mut String,
    last: &mut Option<Class>,
    pending_run: &mut bool,
    prefix: UnicodePrefix,
) {
    if prefix == UnicodePrefix::UAmp {
        separate(out, last, pending_run, Class::Word);
        out.push(' ');
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
    use super::{HASH_VERSION, hash, hash_with_version, normalize};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Case {
        a: String,
        b: String,
        same: bool,
        why: String,
    }

    /// The cases are shared with the TypeScript runtime's own test (now run
    /// through `typedpg_wasm`): the two runners must agree on what drift is,
    /// or one database would look drifted to one CLI and not the other.
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

    #[test]
    fn a_carriage_return_ends_a_line_comment() {
        assert_ne!(
            hash("select 1 -- c\rselect 2;"),
            hash("select 1 -- c\rselect 3;")
        );
        // A CRLF line break normalizes as before.
        assert_eq!(normalize("select 1 -- c\r\nfrom t;"), "select 1 from t;");
    }

    // `u&'a'` is one Unicode-escape string; `u & 'a'` is column `u`
    // bitwise-and a string.
    #[test]
    fn a_unicode_escape_string_is_not_an_and() {
        assert_ne!(hash("SELECT u&'a' FROM t;"), hash("SELECT u & 'a' FROM t;"));
        assert_ne!(
            hash(r#"SELECT u&"a" FROM t;"#),
            hash(r#"SELECT u & "a" FROM t;"#)
        );
        // The literal itself normalizes as it always did: stored hashes of
        // migrations using it stay valid.
        assert_eq!(
            normalize(r"SELECT U&'d\0061t' UESCAPE '\' FROM t;"),
            r"select u&'d\0061t' uescape '\' from t;"
        );
        // However `u` and `&` are spaced, they are two tokens.
        assert_eq!(normalize("SELECT u &'a' FROM t;"), "select u& 'a' from t;");
        assert_eq!(
            normalize("SELECT u&/**/'a' FROM t;"),
            "select u& 'a' from t;"
        );
        // `au`, `&&`: no Unicode prefix to keep apart from.
        assert_eq!(
            normalize("SELECT au & 'a' FROM t;"),
            "select au&'a' from t;"
        );
        assert_eq!(
            normalize("SELECT u && 'a' FROM t;"),
            "select u&&'a' from t;"
        );
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

    const LF_FUNCTION: &str = "CREATE FUNCTION f() RETURNS text AS $$\nBEGIN\n  RETURN 'a\nb';\nEND\n$$ LANGUAGE plpgsql;\n";
    const CRLF_FUNCTION: &str = "CREATE FUNCTION f() RETURNS text AS $$\r\nBEGIN\r\n  RETURN 'a\r\nb';\r\nEND\r\n$$ LANGUAGE plpgsql;\r\n";

    /// Version 1 is what typedpg 0.6 to 0.8 stored: the digests below were
    /// computed by that release's `hash`, and must not change, or rows it
    /// wrote without their text would read as drifted.
    #[test]
    fn version_1_hashes_are_the_released_ones() {
        assert_eq!(
            hash_with_version(LF_FUNCTION, 1).as_deref(),
            Some("36008b21f609949ea9d09c40c9aad1d0")
        );
        assert_eq!(
            hash_with_version(CRLF_FUNCTION, 1).as_deref(),
            Some("7bfec4f9fc4133c7e9feb3b6344748eb")
        );
        assert_eq!(
            hash_with_version("SELECT\x0b1;", 1).as_deref(),
            Some("c36edb0c2f8e8be0f92c9a21f50d831e")
        );
        assert_eq!(
            hash_with_version("SELECT 'x\ry';", 1).as_deref(),
            Some("d27130028390f98d57d559243b9074fc")
        );
        assert_eq!(hash_with_version("SELECT 1;", 0), None);
        assert_eq!(hash_with_version("SELECT 1;", HASH_VERSION + 1), None);
    }

    /// The current version: line endings and PostgreSQL's whitespace are no
    /// edit, and a file with neither hashes as it did under version 1.
    #[test]
    fn line_endings_and_vertical_tabs_are_not_drift() {
        assert_eq!(hash(CRLF_FUNCTION), hash(LF_FUNCTION));
        assert_eq!(
            hash(&CRLF_FUNCTION.replace("\r\n", "\r")),
            hash(LF_FUNCTION)
        );
        assert_eq!(hash(LF_FUNCTION), "36008b21f609949ea9d09c40c9aad1d0");
        assert_eq!(
            hash_with_version(CRLF_FUNCTION, HASH_VERSION),
            Some(hash(LF_FUNCTION))
        );
        assert_eq!(normalize("SELECT\x0b1\x0bFROM\x0ct;"), "select 1 from t;");
        // A line break is still content: removing one is an edit.
        assert_ne!(hash("SELECT 'x\r\ny';"), hash("SELECT 'xy';"));
    }
}
