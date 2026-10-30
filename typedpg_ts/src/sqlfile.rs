//! The queries of a `.sql` file.
//!
//! ```sql
//! /* @name userById */
//! SELECT id, name FROM users WHERE id = $id;
//!
//! /* @name renameUser */
//! UPDATE users SET name = $name WHERE id = $id
//! ```
//!
//! Each query is a `/* @name x */` comment and the one statement after it,
//! up to its `;` (optional for the last one), with the same `$name`
//! parameters and `$..spreads` as `sql("...")`. The file is read as
//! PostgreSQL's lexer reads it — a `;` inside a string, a quoted
//! identifier, a dollar-quoted body, a comment or parentheses ends nothing —
//! and every query keeps its byte offset in the file, so an error points
//! into the file.

use std::collections::HashSet;
use std::ops::Range;

use crate::scan::{LineIndex, Position};

/// A query of a `.sql` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedQuery {
    /// The name after `@name`: the module's export.
    pub name: String,
    /// Where the name is in the file (bytes).
    pub name_span: Range<usize>,
    /// The statement, without its `;`, comments before it and surrounding
    /// whitespace.
    pub text: String,
    /// Where `text` starts in the file (bytes).
    pub start: usize,
    pub pos: Position,
}

/// A mistake in the file's layout, at a byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlDiagnostic {
    pub offset: usize,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqlFile {
    pub queries: Vec<NamedQuery>,
    pub diagnostics: Vec<SqlDiagnostic>,
    pub lines: LineIndex,
}

/// Words a query can't be named: they can't be a `const`'s name.
const RESERVED: &[&str] = &[
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "arguments",
    "eval",
];

/// The block being read.
struct Block {
    /// Whether the query is one of the file's: its name is valid and new.
    exported: bool,
    name: String,
    name_span: Range<usize>,
    /// The statement's first and past-its-last byte, once it has a token.
    text: Option<(usize, usize)>,
    /// Its `;` was read: anything but comments after it is an error.
    terminated: bool,
}

struct Parser<'a> {
    source: &'a str,
    lines: LineIndex,
    queries: Vec<NamedQuery>,
    diagnostics: Vec<SqlDiagnostic>,
    names: HashSet<String>,
    block: Option<Block>,
    /// A stray statement was reported since the last `@name`: one error
    /// for it, not one per token.
    stray_reported: bool,
    paren_depth: usize,
}

/// Read `source`, a `.sql` file.
pub fn parse(source: &str) -> SqlFile {
    let mut p = Parser {
        source,
        lines: LineIndex::new(source),
        queries: Vec::new(),
        diagnostics: Vec::new(),
        names: HashSet::new(),
        block: None,
        stray_reported: false,
        paren_depth: 0,
    };
    p.run();
    p.finish_block();
    SqlFile {
        queries: p.queries,
        diagnostics: p.diagnostics,
        lines: p.lines,
    }
}

impl Parser<'_> {
    fn run(&mut self) {
        let bytes = self.source.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                // scan.l's `non_newline` is `[^\n\r]`: a lone CR ends a
                // `--` comment too.
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    while i < bytes.len() && !matches!(bytes[i], b'\n' | b'\r') {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    let end = skip_block_comment(bytes, i);
                    if end == bytes.len() && !self.source[i..].ends_with("*/") {
                        self.error(i, "unterminated /* comment");
                    }
                    self.comment(i, end);
                    i = end;
                }
                b'\'' | b'"' => {
                    let end = skip_quoted(bytes, i, b, false);
                    self.token(i, end, b);
                    i = end;
                }
                b'$' => {
                    let end = dollar_quote_end(bytes, i).unwrap_or(i + 1);
                    self.token(i, end, b);
                    i = end;
                }
                _ if is_ident_start(b) => {
                    let mut end = i;
                    while end < bytes.len() && is_ident_cont(bytes[end]) {
                        end += 1;
                    }
                    // `E'…'`: a string where backslash escapes.
                    if self.source[i..end].eq_ignore_ascii_case("e")
                        && bytes.get(end) == Some(&b'\'')
                    {
                        end = skip_quoted(bytes, end, b'\'', true);
                    }
                    self.token(i, end, b);
                    i = end;
                }
                _ if b.is_ascii_whitespace() => i += 1,
                _ => {
                    // One character (a UTF-8 sequence starts at >= 0x80,
                    // which is an identifier's).
                    self.token(i, i + 1, b);
                    i += 1;
                }
            }
        }
    }

    fn error(&mut self, at: usize, message: impl Into<String>) {
        self.diagnostics.push(SqlDiagnostic {
            offset: at,
            message: message.into(),
        });
    }

    /// The comment `source[start..end]`: a `@name` starts a block.
    fn comment(&mut self, start: usize, end: usize) {
        let inner_start = start + 2;
        let inner_end = if self.source[..end].ends_with("*/") && end >= inner_start + 2 {
            end - 2
        } else {
            end
        };
        let inner = &self.source[inner_start..inner_end];
        let trimmed = inner.trim_start();
        let Some(after_tag) = trimmed.strip_prefix("@name") else {
            return;
        };
        if after_tag
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            // `@names`: not the tag.
            return;
        }
        self.finish_block();
        let tag_at = inner_start + (inner.len() - trimmed.len());
        let rest_at = tag_at + "@name".len();
        let rest = &self.source[rest_at..inner_end];
        let name_at = rest_at + (rest.len() - rest.trim_start().len());
        let name_len = self.source[name_at..inner_end]
            .char_indices()
            .find(|&(k, c)| {
                !(c == '_' || c == '$' || c.is_alphanumeric() && (k > 0 || !c.is_ascii_digit()))
            })
            .map_or(inner_end - name_at, |(k, _)| k);
        let name = &self.source[name_at..name_at + name_len];
        let after = &self.source[name_at + name_len..inner_end];
        if name.is_empty() {
            self.error(tag_at, "`@name` needs a name: `/* @name userById */`");
        }
        if let Some(word) = after.split_whitespace().next().filter(|_| !name.is_empty()) {
            let at = name_at + name_len + (after.len() - after.trim_start().len());
            if word.starts_with('@') {
                self.error(
                    at,
                    format!(
                        "`{word}` is not read by typedpg: a `/* @name */` comment holds the \
                         name only (parameters are `$name` and `$..name` in the SQL)"
                    ),
                );
            } else {
                self.error(
                    at,
                    format!("unexpected `{word}` after the name: a query's name is one identifier"),
                );
            }
        }
        // A query that can't be exported under its name isn't one.
        let mut exported = !name.is_empty();
        if RESERVED.contains(&name) {
            exported = false;
            self.error(
                name_at,
                format!("`{name}` is a reserved word in JavaScript: name the query otherwise"),
            );
        } else if !name.is_empty() && !self.names.insert(name.to_owned()) {
            exported = false;
            self.error(
                name_at,
                format!("a query named `{name}` is already in this file"),
            );
        }
        self.block = Some(Block {
            exported,
            name: name.to_owned(),
            name_span: name_at..name_at + name_len,
            text: None,
            terminated: false,
        });
        self.stray_reported = false;
        self.paren_depth = 0;
    }

    /// A token `source[start..end]` that begins with byte `first`.
    fn token(&mut self, start: usize, end: usize, first: u8) {
        let Some(block) = &mut self.block else {
            if !self.stray_reported {
                self.stray_reported = true;
                self.error(
                    start,
                    "SQL outside a query: each query starts with a `/* @name queryName */` comment",
                );
            }
            return;
        };
        if block.terminated {
            if !self.stray_reported {
                self.stray_reported = true;
                let name = block.name.clone();
                self.error(
                    start,
                    format!(
                        "a second statement after query `{name}`: each query is one statement, \
                         after its own `/* @name */` comment"
                    ),
                );
            }
            return;
        }
        match first {
            b'(' => self.paren_depth += 1,
            b')' => self.paren_depth = self.paren_depth.saturating_sub(1),
            b';' if self.paren_depth == 0 => {
                block.terminated = true;
                return;
            }
            _ => {}
        }
        let text = block.text.get_or_insert((start, end));
        text.1 = end;
    }

    fn finish_block(&mut self) {
        let Some(block) = self.block.take() else {
            return;
        };
        if block.name.is_empty() {
            return;
        }
        match block.text {
            None => self.error(
                block.name_span.start,
                format!(
                    "query `{}` has no statement after its `/* @name */`",
                    block.name
                ),
            ),
            Some(_) if !block.exported => {}
            Some((start, end)) => self.queries.push(NamedQuery {
                name: block.name,
                name_span: block.name_span,
                text: self.source[start..end].to_owned(),
                start,
                pos: self.lines.position(start),
            }),
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit() || b == b'$'
}

/// The index past a quoted run opened at `open` (`'…'` or `"…"`, the quote
/// doubled to escape it; with `backslash`, `\` escapes the next byte too).
fn skip_quoted(bytes: &[u8], open: usize, quote: u8, backslash: bool) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if backslash && b == b'\\' {
            i += 2;
            continue;
        }
        if b == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// The index past a `/* … */` comment opened at `open`; they nest.
fn skip_block_comment(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0;
    let mut i = open;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
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

/// Past the closing delimiter of a dollar quote opened at `open`; `None`
/// for any other `$` (a `$name` parameter, `$1`).
fn dollar_quote_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut i = open + 1;
    if bytes.get(i).is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }
    while i < bytes.len() && bytes[i] != b'$' {
        if !is_ident_cont(bytes[i]) {
            return None;
        }
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let delimiter = &bytes[open..=i];
    let body = i + 1;
    let close = bytes[body..]
        .windows(delimiter.len())
        .position(|w| w == delimiter)?;
    Some(body + close + delimiter.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(f: &SqlFile) -> Vec<(&str, &str)> {
        f.queries
            .iter()
            .map(|q| (q.name.as_str(), q.text.as_str()))
            .collect()
    }

    fn errors(f: &SqlFile) -> Vec<String> {
        f.diagnostics
            .iter()
            .map(|d| {
                let pos = f.lines.position(d.offset);
                format!("{}:{}: {}", pos.line, pos.col, d.message)
            })
            .collect()
    }

    #[test]
    fn queries_with_their_offsets() {
        let src = "-- users\n/* @name userById */\nSELECT id FROM users WHERE id = $id;\n\n/* @name all */ SELECT 1 AS \"a;b\" -- x;\n";
        let f = parse(src);
        assert_eq!(errors(&f), Vec::<String>::new());
        assert_eq!(
            names(&f),
            [
                ("userById", "SELECT id FROM users WHERE id = $id"),
                ("all", "SELECT 1 AS \"a;b\""),
            ]
        );
        let q = &f.queries[0];
        assert_eq!(&src[q.start..q.start + q.text.len()], q.text);
        assert_eq!(&src[q.name_span.clone()], "userById");
        assert_eq!(q.pos, Position { line: 3, col: 1 });
    }

    #[test]
    fn semicolons_that_end_nothing() {
        let src = "/* @name a */\nSELECT ';', $$ ; $$, $f$ $$;$$ $f$, E'\\';', /* ; /* ; */ ; */ (SELECT 1; 2), $id\n;";
        let f = parse(src);
        assert_eq!(errors(&f), Vec::<String>::new());
        assert_eq!(
            f.queries[0].text,
            "SELECT ';', $$ ; $$, $f$ $$;$$ $f$, E'\\';', /* ; /* ; */ ; */ (SELECT 1; 2), $id"
        );
    }

    #[test]
    fn a_carriage_return_ends_a_line_comment() {
        // scan.l's `non_newline` is `[^\n\r]`: what follows a lone CR is
        // SQL again — the rest of the statement, its `;`, the next `@name`.
        let f = parse("/* @name a */ SELECT 1 -- one\r, 2; /* @name b */ SELECT 3 -- x\r");
        assert_eq!(errors(&f), Vec::<String>::new());
        assert_eq!(
            names(&f),
            [("a", "SELECT 1 -- one\r, 2"), ("b", "SELECT 3")]
        );
        // CRLF, as before.
        let f = parse("/* @name a */\r\nSELECT 1 -- one\r\n, 2;\r\n");
        assert_eq!(names(&f), [("a", "SELECT 1 -- one\r\n, 2")]);
        // A stray statement after a lone CR is reported.
        let f = parse("/* @name a */ SELECT 1; -- x\rSELECT 2");
        assert_eq!(
            errors(&f),
            [
                "1:30: a second statement after query `a`: each query is one statement, after its own `/* @name */` comment"
            ]
        );
    }

    #[test]
    fn comments_before_a_query_are_not_its_text() {
        let f = parse("/* @name a */\n-- the users\n/* all */\nSELECT 1 -- trailing\n");
        assert_eq!(names(&f), [("a", "SELECT 1")]);
    }

    #[test]
    fn mistakes() {
        let f = parse(
            "SELECT 0;\n/* @name */ SELECT 1;\n/* @name a @param ids -> (...) */ SELECT 2;\n\
             /* @name a */ SELECT 3; SELECT 4;\n/* @name b */\n/* @name default */ SELECT 5\n\
             /* @name c d */ SELECT 6",
        );
        assert_eq!(
            errors(&f),
            [
                "1:1: SQL outside a query: each query starts with a `/* @name queryName */` comment",
                "2:4: `@name` needs a name: `/* @name userById */`",
                "3:12: `@param` is not read by typedpg: a `/* @name */` comment holds the name only (parameters are `$name` and `$..name` in the SQL)",
                "4:10: a query named `a` is already in this file",
                "4:25: a second statement after query `a`: each query is one statement, after its own `/* @name */` comment",
                "5:10: query `b` has no statement after its `/* @name */`",
                "6:10: `default` is a reserved word in JavaScript: name the query otherwise",
                "7:12: unexpected `d` after the name: a query's name is one identifier",
            ]
        );
        // The second `a` and `default` are not queries of the file: a module
        // can't export them.
        let names: Vec<&str> = f.queries.iter().map(|q| q.name.as_str()).collect();
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn names_that_are_not_the_tag() {
        // `@names` is another word; a block comment elsewhere is a comment.
        let f = parse("/* @name a */ SELECT /* @namesake */ 1");
        assert_eq!(names(&f), [("a", "SELECT /* @namesake */ 1")]);
    }
}
