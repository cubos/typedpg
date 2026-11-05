//! Finding the `sql!` invocations of a Rust source.
//!
//! The scanner sees syntax only: a macro whose path ends in `sql` is a
//! query, and the same grammar as the macro's `QueryInput` reads its
//! arguments up to the SQL string literal. Other macros are descended into
//! on a best-effort basis (`assert!`, `vec!`, `tokio::try_join!` and the
//! like often wrap a query), by parsing their tokens as expressions and as
//! statements. `copy_in!` and every other macro are not queries.

use std::collections::HashSet;

use proc_macro2::{Span, TokenStream};
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Block, Expr, Ident, LitStr, Macro, Token};

/// A query found at one `sql!` site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoundQuery {
    /// The named database (`db = name`), or `None` for the default one.
    pub db: Option<String>,
    /// The literal's cooked value: the SQL the macro analyzes.
    pub text: String,
    /// Where the literal is in the file.
    pub literal: Literal,
}

/// A string literal as written in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Literal {
    /// The byte offset in the file of the literal's contents (after the
    /// opening delimiter).
    pub content_start: usize,
    /// The contents as written, escapes undecoded.
    pub raw: String,
    /// The cooked value (what the macro analyzes).
    pub value: String,
    /// Whether the literal is a raw string (`r"…"`, `r#"…"#`), whose
    /// contents have no escapes to decode.
    pub raw_string: bool,
}

impl Literal {
    /// The byte offset in the file of the character at byte `cooked` of the
    /// literal's value. Past the end maps to the end.
    pub fn source_offset(&self, cooked: usize) -> usize {
        let cooked = cooked.min(self.value.len());
        if self.raw_string {
            self.content_start + cooked
        } else {
            self.content_start + cooked_to_raw(&self.raw, cooked)
        }
    }
}

/// Every `sql!` query in `text`, in source order; `None` when the file does
/// not parse.
pub(crate) fn queries(text: &str) -> Option<Vec<FoundQuery>> {
    let file = syn::parse_file(text).ok()?;
    let mut finder = Finder {
        text,
        queries: Vec::new(),
        seen: HashSet::new(),
    };
    finder.visit_file(&file);
    Some(finder.queries)
}

struct Finder<'a> {
    text: &'a str,
    queries: Vec<FoundQuery>,
    /// The path start of each `sql!` already recorded: parsing a macro's
    /// tokens as both expressions and statements visits it twice.
    seen: HashSet<usize>,
}

impl Finder<'_> {
    fn record(&mut self, mac: &Macro) {
        let start = span_offset(self.text, mac.path.span());
        if !self.seen.insert(start) {
            return;
        }
        let Some((db, sql)) = parse_arguments(mac.tokens.clone()) else {
            return;
        };
        let Some(literal) = literal(self.text, sql.span(), &sql) else {
            return;
        };
        self.queries.push(FoundQuery {
            db,
            text: literal.value.clone(),
            literal,
        });
    }

    /// Visit the queries inside another macro's tokens, best effort.
    fn descend(&mut self, tokens: &TokenStream) {
        if let Ok(expressions) =
            Punctuated::<Expr, Token![,]>::parse_terminated.parse2(tokens.clone())
        {
            for expression in &expressions {
                self.visit_expr(expression);
            }
        }
        if let Ok(statements) = Block::parse_within.parse2(tokens.clone()) {
            for statement in &statements {
                self.visit_stmt(statement);
            }
        }
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_macro(&mut self, mac: &'ast Macro) {
        if mac
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "sql")
        {
            self.record(mac);
        } else {
            self.descend(&mac.tokens);
        }
    }
}

/// `(db, sql)` from `[db = name,] executor, "SQL", …`, reading no further
/// than the literal.
fn parse_arguments(tokens: TokenStream) -> Option<(Option<String>, LitStr)> {
    let parser = |input: ParseStream<'_>| -> syn::Result<(Option<String>, LitStr)> {
        let db = if input.peek(Ident) {
            let fork = input.fork();
            let ident: Ident = fork.parse()?;
            if ident == "db" && fork.peek(Token![=]) {
                let _: Ident = input.parse()?;
                input.parse::<Token![=]>()?;
                let name: Ident = input.parse()?;
                input.parse::<Token![,]>()?;
                Some(name.to_string())
            } else {
                None
            }
        } else {
            None
        };
        let _: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let sql: LitStr = input.parse()?;
        // The parameter assignments after the literal are irrelevant here.
        let _: TokenStream = input.parse()?;
        Ok((db, sql))
    };
    parser.parse2(tokens).ok()
}

/// The literal at byte `start` of `text`, as written.
fn literal(text: &str, span: Span, lit: &LitStr) -> Option<Literal> {
    let start = span_offset(text, span);
    let token = lit.token().to_string();
    let (content_start, raw, raw_string) = if let Some(rest) = token.strip_prefix('r') {
        let hashes = rest.bytes().take_while(|byte| *byte == b'#').count();
        let quote = 1 + hashes;
        if token.as_bytes().get(quote) != Some(&b'"') {
            return None;
        }
        let end = token.len().checked_sub(1 + hashes)?;
        (
            start + quote + 1,
            token.get(quote + 1..end)?.to_owned(),
            true,
        )
    } else {
        if !token.starts_with('"') || !token.ends_with('"') {
            return None;
        }
        (start + 1, token.get(1..token.len() - 1)?.to_owned(), false)
    };
    Some(Literal {
        content_start,
        raw,
        value: lit.value(),
        raw_string,
    })
}

/// The byte offset of a span's start (1-based line, 0-based char column) in
/// `text`.
fn span_offset(text: &str, span: Span) -> usize {
    let start = span.start();
    line_column_to_offset(text, start.line, start.column).unwrap_or(0)
}

fn line_column_to_offset(text: &str, line: usize, column: usize) -> Option<usize> {
    let mut line_start = 0usize;
    let mut current_line = 1usize;
    for (i, c) in text.char_indices() {
        if current_line == line {
            break;
        }
        if c == '\n' {
            current_line += 1;
            line_start = i + 1;
        }
    }
    if current_line != line {
        return None;
    }
    let mut offset = line_start;
    for _ in 0..column {
        offset += text.get(offset..)?.chars().next()?.len_utf8();
    }
    Some(offset)
}

/// The byte offset in `raw` (a normal string's contents as written) of the
/// character at byte `cooked` of its value. Past the end maps to the end.
///
/// Decodes the escapes a Rust string literal has: `\n`, `\r`, `\t`, `\\`,
/// `\0`, `\'`, `\"`, `\xNN`, `\u{…}`, and the line continuation (`\` and
/// the newline and whitespace after it), which produces nothing.
pub(crate) fn cooked_to_raw(raw: &str, cooked: usize) -> usize {
    let mut i = 0usize;
    let mut produced = 0usize;
    while i < raw.len() {
        let c = raw[i..].chars().next().expect("in bounds");
        if c != '\\' {
            if produced >= cooked {
                return i;
            }
            i += c.len_utf8();
            produced += c.len_utf8();
            continue;
        }
        let rest = &raw[i + 1..];
        // A line continuation and its following whitespace produce nothing:
        // they are stepped over whatever the target offset is.
        match rest.chars().next() {
            Some('\n') => {
                i += 2;
                i = skip_whitespace(raw, i);
                continue;
            }
            Some('\r') if rest.starts_with("\r\n") => {
                i += 3;
                i = skip_whitespace(raw, i);
                continue;
            }
            _ => {}
        }
        if produced >= cooked {
            return i;
        }
        match rest.chars().next() {
            Some('n' | 'r' | 't' | '\\' | '0' | '\'' | '"') => {
                i += 2;
                produced += 1;
            }
            Some('x') => {
                let hex = raw.get(i + 2..i + 4).unwrap_or("00");
                let codepoint = u32::from_str_radix(hex, 16).unwrap_or(0);
                i = (i + 4).min(raw.len());
                produced += char::from_u32(codepoint).map_or(1, char::len_utf8);
            }
            Some('u') if raw.get(i + 2..i + 3) == Some("{") => {
                let Some(end) = raw[i + 3..].find('}').map(|end| i + 3 + end) else {
                    i = (i + 2).min(raw.len());
                    produced += 1;
                    continue;
                };
                let codepoint = u32::from_str_radix(&raw[i + 3..end], 16).unwrap_or(0xFFFD);
                let c = char::from_u32(codepoint).unwrap_or('\u{FFFD}');
                i = end + 1;
                produced += c.len_utf8();
            }
            Some(other) => {
                i += 1 + other.len_utf8();
                produced += other.len_utf8();
            }
            None => {
                i += 1;
                produced += 1;
            }
        }
    }
    raw.len()
}

fn skip_whitespace(raw: &str, mut i: usize) -> usize {
    while let Some(c) = raw.get(i..).and_then(|rest| rest.chars().next()) {
        if !c.is_whitespace() {
            break;
        }
        i += c.len_utf8();
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(source: &str) -> Vec<FoundQuery> {
        queries(source).unwrap_or_else(|| panic!("does not parse: {source}"))
    }

    #[test]
    fn finds_queries_and_their_databases() {
        let source = r#"
fn main() {
    let _ = sql!(pool, "SELECT 1", id = 1);
    let _ = typedpg::sql!(pool, "SELECT 2");
    let _ = sql!(db = other, pool, "SELECT 3");
    assert!(sql!(pool, "SELECT 4") == sql!(pool, "SELECT 5"));
    let _ = vec![sql!(pool, "SELECT 6")];
}
"#;
        let found = scan(source);
        let db: Vec<Option<&str>> = found.iter().map(|query| query.db.as_deref()).collect();
        let sql: Vec<&str> = found.iter().map(|query| query.text.as_str()).collect();
        assert_eq!(db, [None, None, Some("other"), None, None, None]);
        assert_eq!(
            sql,
            [
                "SELECT 1", "SELECT 2", "SELECT 3", "SELECT 4", "SELECT 5", "SELECT 6"
            ]
        );
    }

    #[test]
    fn ignores_copy_in_and_non_literal_arguments() {
        let source = r#"
fn main() {
    copy_in!(pool, "users (id)");
    sql!(pool, query);
    sql!(pool, concat!("SELECT 1"));
    sql!(pool, 42);
}
"#;
        assert!(scan(source).is_empty());
    }

    #[test]
    fn cooked_offsets_map_through_every_escape() {
        let source = r#"fn main() {
    let _ = sql!(pool, "a\nb\tc\rd\\e\0f\'g\"h\x41i\u{1F600}j");
}"#;
        let query = &scan(source)[0];
        assert_eq!(query.text, "a\nb\tc\rd\\e\0f'g\"hAi😀j");
        for (needle, written) in [
            ("\n", "\\n"),
            ("\t", "\\t"),
            ("\r", "\\r"),
            ("\\", "\\\\"),
            ("\0", "\\0"),
            ("'", "\\'"),
            ("\"", "\\\""),
            ("A", "\\x41"),
            ("😀", "\\u{1F600}"),
        ] {
            let at = query.text.find(needle).unwrap();
            let file_at = query.literal.source_offset(at);
            assert!(
                source[file_at..].starts_with(written),
                "{needle:?} maps to {:?}, not {written:?}",
                &source[file_at..]
            );
        }
        // The end of the value is the closing quote.
        let end = query.literal.source_offset(query.text.len());
        assert!(source[end..].starts_with('"'));
    }

    #[test]
    fn a_line_continuation_before_the_site_maps_to_the_sql() {
        let source = r#"fn main() {
    let _ = sql!(pool, "SELECT \
        id FROM users WHERE email = NULL");
}
"#;
        let query = &scan(source)[0];
        assert_eq!(query.text, "SELECT id FROM users WHERE email = NULL");
        let at = query.literal.source_offset(query.text.find("id").unwrap());
        assert!(source[at..].starts_with("id FROM"), "{}", &source[at..]);
        let at = query
            .literal
            .source_offset(query.text.find("email").unwrap());
        assert!(source[at..].starts_with("email = NULL"));
        assert_eq!(cooked_to_raw("a\\\n  b", 1), 5);
        assert_eq!(cooked_to_raw("a\\\n  b", 2), 6);
    }

    #[test]
    fn multi_byte_characters_before_the_literal_keep_the_columns_right() {
        let source = r#"fn main() { let _ = ("é", sql!(pool, "SELECT id FROM users WHERE email = NULL")); }"#;
        let query = &scan(source)[0];
        let at = query
            .literal
            .source_offset(query.text.find("email").unwrap());
        assert!(source[at..].starts_with("email = NULL"));
    }

    #[test]
    fn raw_strings_with_hashes_have_no_escapes() {
        let source = r####"fn main() {
    let _ = sql!(pool, r###"SELECT id FROM users WHERE email = NULL"###);
}"####;
        let query = &scan(source)[0];
        assert_eq!(query.text, "SELECT id FROM users WHERE email = NULL");
        assert!(query.literal.raw_string);
        let at = query
            .literal
            .source_offset(query.text.find("email").unwrap());
        assert!(source[at..].starts_with("email = NULL"));
        let end = query.literal.source_offset(query.text.len());
        assert!(source[end..].starts_with("\"###"));
    }

    #[test]
    fn an_unparseable_file_is_skipped() {
        assert!(queries("fn main( {").is_none());
    }
}
