//! Schema-qualified PostgreSQL object names.
//!
//! PostgreSQL identifiers can contain any character when written between
//! double quotes (e.g. `"foo.bar"` is a single identifier named `foo.bar`).
//! This means that naïvely concatenating `schema + "." + name` into a string
//! key is ambiguous: `"foo.bar".baz` and `foo."bar.baz"` would collide.
//!
//! [`QualifiedName`] keeps `schema` and `name` as separate fields so the
//! collision is impossible. The [`Display`] and [`FromStr`] implementations
//! round-trip through PostgreSQL's identifier quoting rules, quoting only
//! when necessary and escaping embedded `"` as `""`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Schema-qualified name of a table, view, type, or other namespaced object.
///
/// Both `schema` and `name` are stored verbatim — no case folding, no
/// quoting. Use [`Display`] (or [`to_string`](ToString::to_string)) to obtain
/// a PostgreSQL-parseable rendering, and [`FromStr`] (or [`parse`](str::parse))
/// to go the other way.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct QualifiedName {
    pub schema: String,
    pub name: String,
}

impl QualifiedName {
    /// Construct a qualified name from schema and object parts.
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
        }
    }

    /// Parse a name that *may* be schema-qualified, following PostgreSQL
    /// identifier rules.
    ///
    /// Unlike [`FromStr`], a bare (single-part) name is accepted and placed in
    /// `default_schema` rather than rejected. Quoting is honoured, so a quoted
    /// identifier containing a `.` stays a single part — `"foo.bar"` parses as
    /// the name `foo.bar` in `default_schema`, not as schema `foo` / name
    /// `bar`. Unquoted identifiers are case-folded to lowercase, as PG does.
    pub(crate) fn parse_in_schema(
        s: &str,
        default_schema: &str,
    ) -> Result<Self, ParseQualifiedNameError> {
        let mut parser = Parser::new(s);
        let first = parser.read_ident()?;
        if parser.peek() == Some('.') {
            parser.bump();
            let name = parser.read_ident()?;
            parser.expect_end()?;
            Ok(QualifiedName {
                schema: first,
                name,
            })
        } else {
            parser.expect_end()?;
            Ok(QualifiedName {
                schema: default_schema.to_string(),
                name: first,
            })
        }
    }
}

impl fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_ident(f, &self.schema)?;
        f.write_str(".")?;
        write_ident(f, &self.name)
    }
}

impl FromStr for QualifiedName {
    type Err = ParseQualifiedNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parser = Parser::new(s);
        let schema = parser.read_ident()?;
        parser.expect_dot()?;
        let name = parser.read_ident()?;
        parser.expect_end()?;
        Ok(QualifiedName { schema, name })
    }
}

impl TryFrom<String> for QualifiedName {
    type Error = ParseQualifiedNameError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<QualifiedName> for String {
    fn from(q: QualifiedName) -> Self {
        q.to_string()
    }
}

/// Errors returned while parsing a qualified name string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseQualifiedNameError {
    #[error("expected identifier at byte {0}")]
    ExpectedIdent(usize),
    #[error("expected '.' at byte {0}")]
    ExpectedDot(usize),
    #[error("expected end of input at byte {0}")]
    ExpectedEnd(usize),
    #[error("unterminated quoted identifier starting at byte {0}")]
    UnterminatedQuote(usize),
    #[error("empty quoted identifier at byte {0}")]
    EmptyQuotedIdent(usize),
}

// ──────────────────────────────────────────────────────────────────────────────
// Ident rendering
// ──────────────────────────────────────────────────────────────────────────────

/// A single identifier rendered the way [`QualifiedName`]'s [`Display`]
/// renders each part: quoted only when necessary (PG's `quote_identifier`).
pub fn quote_identifier(s: &str) -> String {
    struct Ident<'a>(&'a str);
    impl fmt::Display for Ident<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write_ident(f, self.0)
        }
    }
    Ident(s).to_string()
}

fn write_ident(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    if needs_quoting(s) {
        f.write_str("\"")?;
        for ch in s.chars() {
            if ch == '"' {
                f.write_str("\"\"")?;
            } else {
                f.write_str(&ch.to_string())?;
            }
        }
        f.write_str("\"")
    } else {
        f.write_str(s)
    }
}

/// PG's `quote_identifier` (ruleutils.c): an identifier stays bare only
/// when it is a lowercase ASCII letter or `_` followed by lowercase ASCII
/// letters, digits and `_` (so uppercase, `$`, non-ASCII and the empty
/// string are quoted), and isn't a keyword that isn't unreserved.
fn needs_quoting(s: &str) -> bool {
    let mut chars = s.chars();
    let safe = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    !safe || crate::keywords::is_quoted_keyword(s)
}

// ──────────────────────────────────────────────────────────────────────────────
// Parser
// ──────────────────────────────────────────────────────────────────────────────

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn read_ident(&mut self) -> Result<String, ParseQualifiedNameError> {
        match self.peek() {
            Some('"') => self.read_quoted_ident(),
            Some(c) if c.is_ascii_alphabetic() || c == '_' => self.read_unquoted_ident(),
            _ => Err(ParseQualifiedNameError::ExpectedIdent(self.pos)),
        }
    }

    fn read_unquoted_ident(&mut self) -> Result<String, ParseQualifiedNameError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                self.bump();
            } else {
                break;
            }
        }
        // PG folds unquoted identifiers to lowercase.
        Ok(self.src[start..self.pos].to_ascii_lowercase())
    }

    fn read_quoted_ident(&mut self) -> Result<String, ParseQualifiedNameError> {
        let start = self.pos;
        self.bump(); // opening quote
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(ParseQualifiedNameError::UnterminatedQuote(start)),
                Some('"') => {
                    // `""` is an escaped quote.
                    if self.peek() == Some('"') {
                        self.bump();
                        out.push('"');
                    } else {
                        break;
                    }
                }
                Some(c) => out.push(c),
            }
        }
        if out.is_empty() {
            return Err(ParseQualifiedNameError::EmptyQuotedIdent(start));
        }
        Ok(out)
    }

    fn expect_dot(&mut self) -> Result<(), ParseQualifiedNameError> {
        if self.peek() == Some('.') {
            self.bump();
            Ok(())
        } else {
            Err(ParseQualifiedNameError::ExpectedDot(self.pos))
        }
    }

    fn expect_end(&self) -> Result<(), ParseQualifiedNameError> {
        if self.pos == self.src.len() {
            Ok(())
        } else {
            Err(ParseQualifiedNameError::ExpectedEnd(self.pos))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_follows_pg_quote_identifier() {
        for (ident, rendered) in [
            ("user_id", "user_id"),
            ("_x1", "_x1"),
            ("abort", "abort"),     // unreserved keyword
            ("order", "\"order\""), // reserved
            ("int", "\"int\""),     // column-name keyword
            ("left", "\"left\""),   // type/function-name keyword
            ("a$b", "\"a$b\""),
            ("Users", "\"Users\""),
            ("1a", "\"1a\""),
            ("é", "\"é\""),
            ("", "\"\""),
            ("a\"b", "\"a\"\"b\""),
        ] {
            assert_eq!(quote_identifier(ident), rendered, "{ident:?}");
        }
        let q = QualifiedName::new("select", "order");
        assert_eq!(q.to_string(), "\"select\".\"order\"");
        assert_eq!(q.to_string().parse::<QualifiedName>().unwrap(), q);
    }

    #[test]
    fn parse_simple() {
        let q: QualifiedName = "public.users".parse().unwrap();
        assert_eq!(q.schema, "public");
        assert_eq!(q.name, "users");
    }

    #[test]
    fn parse_case_folding_unquoted() {
        let q: QualifiedName = "Public.Users".parse().unwrap();
        assert_eq!(q.schema, "public");
        assert_eq!(q.name, "users");
    }

    #[test]
    fn parse_quoted_preserves_case() {
        let q: QualifiedName = "\"Public\".\"Users\"".parse().unwrap();
        assert_eq!(q.schema, "Public");
        assert_eq!(q.name, "Users");
    }

    #[test]
    fn parse_quoted_with_dots() {
        let q: QualifiedName = "\"foo.bar\".baz".parse().unwrap();
        assert_eq!(q.schema, "foo.bar");
        assert_eq!(q.name, "baz");
    }

    #[test]
    fn parse_escaped_quote() {
        let q: QualifiedName = "\"a\"\"b\".c".parse().unwrap();
        assert_eq!(q.schema, "a\"b");
        assert_eq!(q.name, "c");
    }

    #[test]
    fn parse_mixed_quoting() {
        let q: QualifiedName = "public.\"MyTable\"".parse().unwrap();
        assert_eq!(q.schema, "public");
        assert_eq!(q.name, "MyTable");
    }

    #[test]
    fn parse_missing_schema_errors() {
        let err = "users".parse::<QualifiedName>().unwrap_err();
        assert!(matches!(err, ParseQualifiedNameError::ExpectedDot(_)));
    }

    #[test]
    fn parse_in_schema_bare_name_uses_default() {
        let q = QualifiedName::parse_in_schema("user_role", "public").unwrap();
        assert_eq!(q, QualifiedName::new("public", "user_role"));
    }

    #[test]
    fn parse_in_schema_qualified_name() {
        let q = QualifiedName::parse_in_schema("geo.polygon", "public").unwrap();
        assert_eq!(q, QualifiedName::new("geo", "polygon"));
    }

    #[test]
    fn parse_in_schema_quoted_name_with_dot_is_one_part() {
        // A quoted identifier containing a `.` is a single name, not a
        // schema-qualified pair.
        let q = QualifiedName::parse_in_schema("\"foo.bar\"", "public").unwrap();
        assert_eq!(q, QualifiedName::new("public", "foo.bar"));
    }

    #[test]
    fn parse_in_schema_quoted_schema_with_dot() {
        let q = QualifiedName::parse_in_schema("\"foo.bar\".baz", "public").unwrap();
        assert_eq!(q, QualifiedName::new("foo.bar", "baz"));
    }

    #[test]
    fn parse_in_schema_folds_unquoted_case() {
        let q = QualifiedName::parse_in_schema("MyType", "public").unwrap();
        assert_eq!(q, QualifiedName::new("public", "mytype"));
    }

    #[test]
    fn parse_in_schema_preserves_quoted_case() {
        let q = QualifiedName::parse_in_schema("\"MyType\"", "public").unwrap();
        assert_eq!(q, QualifiedName::new("public", "MyType"));
    }

    #[test]
    fn parse_in_schema_rejects_trailing_garbage() {
        assert!(QualifiedName::parse_in_schema("a.b.c", "public").is_err());
        assert!(QualifiedName::parse_in_schema("", "public").is_err());
    }

    #[test]
    fn parse_trailing_garbage_errors() {
        let err = "public.users.extra".parse::<QualifiedName>().unwrap_err();
        assert!(matches!(err, ParseQualifiedNameError::ExpectedEnd(_)));
    }

    #[test]
    fn parse_unterminated_quote_errors() {
        let err = "\"unclosed.users".parse::<QualifiedName>().unwrap_err();
        assert!(matches!(err, ParseQualifiedNameError::UnterminatedQuote(_)));
    }

    #[test]
    fn parse_empty_quoted_errors() {
        let err = "\"\".users".parse::<QualifiedName>().unwrap_err();
        assert!(matches!(err, ParseQualifiedNameError::EmptyQuotedIdent(_)));
    }

    #[test]
    fn display_simple() {
        let q = QualifiedName::new("public", "users");
        assert_eq!(q.to_string(), "public.users");
    }

    #[test]
    fn display_quotes_when_uppercase() {
        let q = QualifiedName::new("public", "MyTable");
        assert_eq!(q.to_string(), "public.\"MyTable\"");
    }

    #[test]
    fn display_quotes_when_contains_dot() {
        let q = QualifiedName::new("foo.bar", "baz");
        assert_eq!(q.to_string(), "\"foo.bar\".baz");
    }

    #[test]
    fn display_escapes_embedded_quote() {
        let q = QualifiedName::new("a\"b", "c");
        assert_eq!(q.to_string(), "\"a\"\"b\".c");
    }

    #[test]
    fn roundtrip_exotic() {
        let q = QualifiedName::new("foo.bar", "baz qux");
        let rendered = q.to_string();
        let parsed: QualifiedName = rendered.parse().unwrap();
        assert_eq!(parsed, q);
    }

    #[test]
    fn serde_json_roundtrip() {
        let q = QualifiedName::new("foo.bar", "baz");
        let json = serde_json::to_string(&q).unwrap();
        assert_eq!(json, "\"\\\"foo.bar\\\".baz\"");
        let back: QualifiedName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, q);
    }

    #[test]
    fn serde_json_accepts_legacy_bare_format() {
        // Seed.json-style keys (no characters requiring quoting) parse fine.
        let q: QualifiedName = serde_json::from_str("\"public.users\"").unwrap();
        assert_eq!(q, QualifiedName::new("public", "users"));
    }
}
