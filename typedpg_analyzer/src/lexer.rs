//! SQL template lexer for `typedpg`.
//!
//! This module provides the [`lex`] function, which takes a SQL string
//! containing `$name` parameters and `$..spread` syntax and produces a
//! [`LexOutput`] with the SQL rewritten to use PostgreSQL positional
//! placeholders (`$1`, `$2`, ...).
//!
//! The lexer is **not** a full SQL parser. It tracks just enough state to
//! distinguish between normal SQL context and string literals, comments,
//! dollar-quoted strings, and quoted identifiers. Parameters are only
//! extracted in normal context -- `$name` inside a string literal or comment
//! is left untouched.

use crate::param::{LexOutput, Param, Rewrite, SpreadField, SpreadParam};

/// An error produced when the lexer encounters invalid SQL syntax.
///
/// All variants include the byte `position` where the problem was detected.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum LexError {
    /// A single-quoted string literal was opened but never closed.
    UnclosedString { position: usize },
    /// A `/* ... */` block comment was opened but never closed.
    UnclosedBlockComment { position: usize },
    /// A dollar-quoted string (`$$...$$` or `$tag$...$tag$`) was opened but
    /// never closed.
    UnclosedDollarQuote { tag: String, position: usize },
    /// A double-quoted identifier (`"..."`) was opened but never closed.
    UnclosedQuotedIdentifier { position: usize },
}

impl LexError {
    /// The byte offset in the original SQL where the unclosed token started.
    pub(crate) fn position(&self) -> usize {
        match self {
            Self::UnclosedString { position }
            | Self::UnclosedBlockComment { position }
            | Self::UnclosedDollarQuote { position, .. }
            | Self::UnclosedQuotedIdentifier { position } => *position,
        }
    }
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnclosedString { position } => {
                write!(f, "unclosed string literal at byte {position}")
            }
            Self::UnclosedBlockComment { position } => {
                write!(f, "unclosed block comment at byte {position}")
            }
            Self::UnclosedDollarQuote { tag, position } => {
                write!(f, "unclosed dollar-quote ${tag}$ at byte {position}")
            }
            Self::UnclosedQuotedIdentifier { position } => {
                write!(f, "unclosed quoted identifier at byte {position}")
            }
        }
    }
}

impl std::error::Error for LexError {}

/// Internal state machine states for the lexer.
enum LexState {
    Normal,
    /// Inside a single-quoted string; the bool marks an `E'…'` escape
    /// string, where a backslash escapes the next character (so `\'` does
    /// not close it).
    StringLiteral(usize, bool),
    DollarQuote(String, usize),
    LineComment,
    /// Block comment with nesting depth and start position of the outermost `/*`.
    BlockComment(usize, usize),
    QuotedIdentifier(usize),
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn read_ident(chars: &[char], start: usize) -> &[char] {
    let mut end = start;
    while end < chars.len() && is_ident_char(chars[end]) {
        end += 1;
    }
    &chars[start..end]
}

/// Lex a SQL template string, extracting named and spread parameters.
///
/// Rewrites `$name` placeholders to positional PostgreSQL placeholders
/// (`$1`, `$2`, ...) and removes `$..spread` tokens, recording their byte
/// offsets for later expansion by the code generator.
///
/// Parameters inside string literals, comments, dollar-quoted strings, and
/// quoted identifiers are ignored.
///
/// # Errors
///
/// Returns a [`LexError`] if the SQL contains an unclosed string literal,
/// block comment, dollar-quoted string, or quoted identifier.
///
/// # Examples
///
/// ```ignore
/// let output = lex("SELECT * FROM users WHERE id = $id AND name = $name").unwrap();
/// assert_eq!(output.sql, "SELECT * FROM users WHERE id = $1 AND name = $2");
/// assert_eq!(output.params.len(), 2);
/// assert_eq!(output.params[0].name, "id");
/// assert_eq!(output.params[1].name, "name");
/// ```
pub(crate) fn lex(sql: &str) -> Result<LexOutput, LexError> {
    // Native `$N` placeholders keep their numbers, so named parameters are
    // numbered after the highest one: lex once to find it, and again with
    // that base when both kinds are present.
    let (out, max_native) = lex_with_base(sql, 0)?;
    let only_native = out.params.iter().all(|p| p.name.parse::<usize>().is_ok());
    if max_native == 0 || only_native {
        return Ok(out);
    }
    Ok(lex_with_base(sql, max_native)?.0)
}

/// Highest native placeholder number PG accepts (`MaxAllocSize` bounds the
/// parameter array; PG's wire protocol caps a statement at 65535).
const MAX_NATIVE_PARAM: usize = 65535;

/// [`lex`] with named parameters numbered from `base + 1`. Native `$N`
/// placeholders are kept as written and fill param slots `1..=N` (named
/// after their number). Returns the output and the highest native number.
fn lex_with_base(sql: &str, base: usize) -> Result<(LexOutput, usize), LexError> {
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut state = LexState::Normal;
    let mut out = String::with_capacity(sql.len());
    let mut params: Vec<Param> = Vec::new();
    let mut spreads: Vec<SpreadParam> = Vec::new();
    let mut rewrites: Vec<Rewrite> = Vec::new();
    // Map from param name to its 1-based positional index
    let mut param_indices: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut max_native = 0usize;
    // Slots `1..=base` belong to native placeholders.
    for n in 1..=base {
        params.push(Param {
            name: n.to_string(),
            nullable: None,
            sql_offsets: Vec::new(),
        });
    }
    let mut i = 0;
    // Track byte position incrementally (avoids a separate Vec<usize> allocation).
    let char_byte_lens: Vec<u8> = sql.chars().map(|c| c.len_utf8() as u8).collect();
    let byte_offset_of =
        |idx: usize, lens: &[u8]| -> usize { lens[..idx].iter().map(|&b| b as usize).sum() };

    while i < len {
        match &state {
            LexState::Normal => {
                if chars[i] == '\'' {
                    // `E'…'` / `e'…'` escape strings: backslash escapes the
                    // next char. The prefix only counts when the E is its own
                    // token (not the tail of an identifier like `tablE`).
                    let escape = i > 0
                        && matches!(chars[i - 1], 'E' | 'e')
                        && (i < 2 || !is_ident_char(chars[i - 2]));
                    state = LexState::StringLiteral(byte_offset_of(i, &char_byte_lens), escape);
                    out.push('\'');
                    i += 1;
                } else if chars[i] == '"' {
                    state = LexState::QuotedIdentifier(byte_offset_of(i, &char_byte_lens));
                    out.push('"');
                    i += 1;
                } else if chars[i] == '-' && i + 1 < len && chars[i + 1] == '-' {
                    state = LexState::LineComment;
                    out.push('-');
                    out.push('-');
                    i += 2;
                } else if chars[i] == '/' && i + 1 < len && chars[i + 1] == '*' {
                    state = LexState::BlockComment(1, byte_offset_of(i, &char_byte_lens));
                    out.push('/');
                    out.push('*');
                    i += 2;
                } else if chars[i] == '$' {
                    let pos = byte_offset_of(i, &char_byte_lens);
                    // Check for spread: $..
                    if i + 2 < len && chars[i + 1] == '.' && chars[i + 2] == '.' {
                        let ident_start = i + 3;
                        if ident_start < len && is_ident_start(chars[ident_start]) {
                            let ident_chars = read_ident(&chars, ident_start);
                            let name: String = ident_chars.iter().collect();
                            let after_ident = ident_start + ident_chars.len();
                            // Check for optional { fields }
                            let mut fi = after_ident;
                            while fi < len && chars[fi].is_ascii_whitespace() {
                                fi += 1;
                            }
                            let out_offset = out.len();
                            if fi < len && chars[fi] == '{' {
                                fi += 1; // skip {
                                let mut fields = Vec::new();
                                loop {
                                    while fi < len && chars[fi].is_ascii_whitespace() {
                                        fi += 1;
                                    }
                                    if fi >= len {
                                        break;
                                    }
                                    if chars[fi] == '}' {
                                        fi += 1;
                                        break;
                                    }
                                    if is_ident_start(chars[fi]) {
                                        let fc = read_ident(&chars, fi);
                                        let field_name: String = fc.iter().collect();
                                        fi += fc.len();
                                        // Check for nullable annotation `?` on field.
                                        let field_nullable = fi < len && chars[fi] == '?';
                                        if field_nullable {
                                            fi += 1;
                                        }
                                        fields.push(SpreadField {
                                            name: field_name,
                                            nullable: field_nullable,
                                        });
                                        while fi < len && chars[fi].is_ascii_whitespace() {
                                            fi += 1;
                                        }
                                        if fi < len && chars[fi] == ',' {
                                            fi += 1;
                                        }
                                    } else {
                                        fi += 1;
                                    }
                                }
                                // Don't emit spread token — just record the offset
                                let original_at = byte_offset_of(i, &char_byte_lens);
                                let original_end = byte_offset_of(fi, &char_byte_lens);
                                rewrites.push(Rewrite {
                                    original_at,
                                    post_lex_at: out_offset,
                                    original_len: original_end - original_at,
                                    post_lex_len: 0,
                                });
                                spreads.push(SpreadParam {
                                    name,
                                    fields: Some(fields),
                                    offset: out_offset,
                                });
                                i = fi;
                            } else {
                                let original_at = byte_offset_of(i, &char_byte_lens);
                                let original_end = byte_offset_of(after_ident, &char_byte_lens);
                                rewrites.push(Rewrite {
                                    original_at,
                                    post_lex_at: out_offset,
                                    original_len: original_end - original_at,
                                    post_lex_len: 0,
                                });
                                spreads.push(SpreadParam {
                                    name,
                                    fields: None,
                                    offset: out_offset,
                                });
                                i = after_ident;
                            }
                        } else {
                            out.push('$');
                            i += 1;
                        }
                    } else if i + 1 < len && chars[i + 1] == '$' {
                        // Dollar-quote $$
                        state = LexState::DollarQuote(String::new(), pos);
                        out.push('$');
                        out.push('$');
                        i += 2;
                    } else if i + 1 < len && is_ident_start(chars[i + 1]) {
                        // Could be $tag$ (dollar-quote) or $param
                        let ident_chars = read_ident(&chars, i + 1);
                        let ident: String = ident_chars.iter().collect();
                        let after = i + 1 + ident_chars.len();
                        if after < len && chars[after] == '$' {
                            // Dollar-quote $tag$
                            state = LexState::DollarQuote(ident.clone(), pos);
                            let span: String = chars[i..=after].iter().collect();
                            out.push_str(&span);
                            i = after + 1;
                        } else {
                            // Named param $ident — check for nullability annotation `?` or `!`
                            let (nullable, consume_to) = if after < len && chars[after] == '?' {
                                (Some(true), after + 1)
                            } else if after < len && chars[after] == '!' {
                                (Some(false), after + 1)
                            } else {
                                (None, after)
                            };

                            // Deduplicate by name
                            let next_idx = if let Some(&idx) = param_indices.get(&ident) {
                                idx
                            } else {
                                let idx = base + param_indices.len() + 1;
                                param_indices.insert(ident.clone(), idx);
                                params.push(Param {
                                    name: ident,
                                    nullable,
                                    sql_offsets: Vec::new(),
                                });
                                idx
                            };
                            let original_at = byte_offset_of(i, &char_byte_lens);
                            let original_end = byte_offset_of(consume_to, &char_byte_lens);
                            let post_lex_at = out.len();
                            let placeholder = format!("${next_idx}");
                            out.push_str(&placeholder);
                            rewrites.push(Rewrite {
                                original_at,
                                post_lex_at,
                                original_len: original_end - original_at,
                                post_lex_len: placeholder.len(),
                            });
                            // Record the byte offset right after this placeholder.
                            params[next_idx - 1].sql_offsets.push(out.len());
                            i = consume_to;
                        }
                    } else if i + 1 < len && chars[i + 1].is_ascii_digit() {
                        // Native PG placeholder `$N`: kept as written; it is
                        // parameter N like any other.
                        let mut end = i + 1;
                        while end < len && chars[end].is_ascii_digit() {
                            end += 1;
                        }
                        let digits: String = chars[i + 1..end].iter().collect();
                        out.push('$');
                        out.push_str(&digits);
                        match digits.parse::<usize>() {
                            Ok(n) if (1..=MAX_NATIVE_PARAM).contains(&n) => {
                                max_native = max_native.max(n);
                                // In the first pass (base 0) the slots are
                                // only created when no named param exists.
                                while params.len() < n && param_indices.is_empty() {
                                    params.push(Param {
                                        name: (params.len() + 1).to_string(),
                                        nullable: None,
                                        sql_offsets: Vec::new(),
                                    });
                                }
                                if let Some(p) = params.get_mut(n - 1)
                                    && p.name == n.to_string()
                                {
                                    p.sql_offsets.push(out.len());
                                }
                            }
                            _ => {}
                        }
                        i = end;
                    } else {
                        // Literal `$`.
                        out.push('$');
                        i += 1;
                    }
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            LexState::StringLiteral(start_pos, escape) => {
                let start_pos = *start_pos;
                let escape = *escape;
                if escape && chars[i] == '\\' && i + 1 < len {
                    // Escape string: the backslash consumes the next char
                    // (`\'`, `\\`, …) without affecting string state.
                    out.push(chars[i]);
                    out.push(chars[i + 1]);
                    i += 2;
                } else if chars[i] == '\'' {
                    if i + 1 < len && chars[i + 1] == '\'' {
                        out.push('\'');
                        out.push('\'');
                        i += 2;
                    } else {
                        out.push('\'');
                        state = LexState::Normal;
                        i += 1;
                    }
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
                if i >= len
                    && let LexState::StringLiteral(..) = state
                {
                    return Err(LexError::UnclosedString {
                        position: start_pos,
                    });
                }
            }
            LexState::DollarQuote(tag, start_pos) => {
                let tag = tag.clone();
                let start_pos = *start_pos;
                if chars[i] == '$' {
                    if tag.is_empty() {
                        if i + 1 < len && chars[i + 1] == '$' {
                            out.push('$');
                            out.push('$');
                            state = LexState::Normal;
                            i += 2;
                            continue;
                        }
                    } else {
                        let close: Vec<char> = format!("${tag}$").chars().collect();
                        if i + close.len() <= len && chars[i..i + close.len()] == close[..] {
                            let s: String = close.iter().collect();
                            out.push_str(&s);
                            state = LexState::Normal;
                            i += close.len();
                            continue;
                        }
                    }
                }
                out.push(chars[i]);
                i += 1;
                if i >= len
                    && let LexState::DollarQuote(_, _) = state
                {
                    return Err(LexError::UnclosedDollarQuote {
                        tag,
                        position: start_pos,
                    });
                }
            }
            LexState::LineComment => {
                out.push(chars[i]);
                if chars[i] == '\n' {
                    state = LexState::Normal;
                }
                i += 1;
            }
            LexState::BlockComment(depth, start_pos) => {
                let depth = *depth;
                let start_pos = *start_pos;
                if chars[i] == '/' && i + 1 < len && chars[i + 1] == '*' {
                    // Nested block comment
                    out.push('/');
                    out.push('*');
                    state = LexState::BlockComment(depth + 1, start_pos);
                    i += 2;
                } else if chars[i] == '*' && i + 1 < len && chars[i + 1] == '/' {
                    out.push('*');
                    out.push('/');
                    if depth == 1 {
                        state = LexState::Normal;
                    } else {
                        state = LexState::BlockComment(depth - 1, start_pos);
                    }
                    i += 2;
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
                if i >= len
                    && let LexState::BlockComment(_, _) = state
                {
                    return Err(LexError::UnclosedBlockComment {
                        position: start_pos,
                    });
                }
            }
            LexState::QuotedIdentifier(start_pos) => {
                let start_pos = *start_pos;
                if chars[i] == '"' {
                    if i + 1 < len && chars[i + 1] == '"' {
                        // Escaped double-quote inside quoted identifier
                        out.push('"');
                        out.push('"');
                        i += 2;
                    } else {
                        out.push('"');
                        state = LexState::Normal;
                        i += 1;
                    }
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
                if i >= len
                    && let LexState::QuotedIdentifier(_) = state
                {
                    return Err(LexError::UnclosedQuotedIdentifier {
                        position: start_pos,
                    });
                }
            }
        }
    }

    match &state {
        LexState::Normal | LexState::LineComment => {}
        LexState::StringLiteral(p, _) => return Err(LexError::UnclosedString { position: *p }),
        LexState::DollarQuote(tag, p) => {
            return Err(LexError::UnclosedDollarQuote {
                tag: tag.clone(),
                position: *p,
            });
        }
        LexState::BlockComment(_, p) => {
            return Err(LexError::UnclosedBlockComment { position: *p });
        }
        LexState::QuotedIdentifier(p) => {
            return Err(LexError::UnclosedQuotedIdentifier { position: *p });
        }
    }

    Ok((
        LexOutput {
            sql: out,
            params,
            spreads,
            rewrites,
        },
        max_native,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_params() {
        let out = lex("SELECT 1").unwrap();
        assert_eq!(out.sql, "SELECT 1");
        assert!(out.params.is_empty());
        assert!(out.spreads.is_empty());
    }

    #[test]
    fn single_param() {
        let out = lex("SELECT * FROM users WHERE id = $id").unwrap();
        assert_eq!(out.sql, "SELECT * FROM users WHERE id = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn multiple_params() {
        let out = lex("INSERT INTO t (a, b, c) VALUES ($a, $b, $c)").unwrap();
        assert_eq!(out.sql, "INSERT INTO t (a, b, c) VALUES ($1, $2, $3)");
        assert_eq!(out.params.len(), 3);
        assert_eq!(out.params[0].name, "a");
        assert_eq!(out.params[1].name, "b");
        assert_eq!(out.params[2].name, "c");
    }

    #[test]
    fn repeated_param_is_deduplicated() {
        let out = lex("SELECT * FROM t WHERE a = $x AND b = $x").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE a = $1 AND b = $1");
        // params has only one entry for "x"
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "x");
    }

    #[test]
    fn repeated_param_mixed_with_others() {
        let out = lex("SELECT $a, $b, $a, $c, $b").unwrap();
        assert_eq!(out.sql, "SELECT $1, $2, $1, $3, $2");
        assert_eq!(out.params.len(), 3);
        assert_eq!(out.params[0].name, "a");
        assert_eq!(out.params[1].name, "b");
        assert_eq!(out.params[2].name, "c");
    }

    #[test]
    fn param_in_string_literal_ignored() {
        let out = lex("SELECT '$falso' FROM t WHERE id = $id").unwrap();
        assert_eq!(out.sql, "SELECT '$falso' FROM t WHERE id = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_line_comment_ignored() {
        let out = lex("SELECT 1 -- $comentario\nWHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_block_comment_ignored() {
        let out = lex("SELECT /* $comentario */ 1 WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_dollar_quote_ignored() {
        let out = lex("SELECT $$body $falso$$ WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_tagged_dollar_quote_ignored() {
        let out = lex("SELECT $fn$body $falso$fn$ WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_quoted_identifier_ignored() {
        let out = lex("SELECT \"$coluna\" FROM t WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn string_with_escape() {
        let out = lex("SELECT 'it''s $falso' FROM t WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
        assert!(out.sql.contains("'it''s $falso'"));
    }

    #[test]
    fn spread_without_fields() {
        let out = lex("INSERT INTO t VALUES $..items").unwrap();
        assert!(out.params.is_empty());
        assert_eq!(out.spreads.len(), 1);
        assert_eq!(out.spreads[0].name, "items");
        assert_eq!(out.spreads[0].fields, None);
        // Spread is removed from output SQL
        assert_eq!(out.sql, "INSERT INTO t VALUES ");
        assert_eq!(out.spreads[0].offset, out.sql.len());
    }

    #[test]
    fn spread_with_fields() {
        let out = lex("INSERT INTO t VALUES $..items { name, email, age }").unwrap();
        assert_eq!(out.spreads.len(), 1);
        assert_eq!(out.spreads[0].name, "items");
        assert_eq!(
            out.spreads[0].fields,
            Some(vec!["name".into(), "email".into(), "age".into()])
        );
        assert_eq!(out.sql, "INSERT INTO t VALUES ");
    }

    #[test]
    fn spread_offset_after_param() {
        let out = lex("INSERT INTO t (org) VALUES ($org), $..items { name }").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "org");
        assert_eq!(out.spreads.len(), 1);
        assert_eq!(out.spreads[0].name, "items");
        // $org becomes $1, then spread is removed
        assert_eq!(out.sql, "INSERT INTO t (org) VALUES ($1), ");
        assert_eq!(out.spreads[0].offset, out.sql.len());
    }

    #[test]
    fn multiple_spreads() {
        let out = lex("INSERT INTO t VALUES $..a { x }, $..b { y }").unwrap();
        assert_eq!(out.spreads.len(), 2);
        assert_eq!(out.spreads[0].name, "a");
        assert_eq!(out.spreads[1].name, "b");
        // Both spreads removed, offsets mark insertion points
        assert_eq!(out.sql, "INSERT INTO t VALUES , ");
        assert!(out.spreads[0].offset < out.spreads[1].offset);
    }

    #[test]
    fn native_pg_placeholder_preserved() {
        let out = lex("SELECT * FROM t WHERE id = $1 AND name = $2").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE id = $1 AND name = $2");
        let names: Vec<&str> = out.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["1", "2"]);
    }

    #[test]
    fn named_params_are_numbered_after_native_ones() {
        let out = lex("SELECT $name, $2, $name, $other").unwrap();
        assert_eq!(out.sql, "SELECT $3, $2, $3, $4");
        let names: Vec<&str> = out.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["1", "2", "name", "other"]);
    }

    #[test]
    fn unclosed_string_error() {
        let err = lex("SELECT 'unclosed").unwrap_err();
        assert!(matches!(err, LexError::UnclosedString { .. }));
    }

    #[test]
    fn unclosed_block_comment_error() {
        let err = lex("SELECT /* unclosed").unwrap_err();
        assert!(matches!(err, LexError::UnclosedBlockComment { .. }));
    }

    #[test]
    fn nullable_param() {
        let out = lex("SELECT * FROM t WHERE age = $age?").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE age = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "age");
        assert_eq!(out.params[0].nullable, Some(true));
    }

    #[test]
    fn force_not_null_param() {
        let out = lex("SELECT * FROM t WHERE age = $age!").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE age = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "age");
        assert_eq!(out.params[0].nullable, Some(false));
    }

    #[test]
    fn non_nullable_param_default() {
        let out = lex("SELECT * FROM t WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
        assert_eq!(out.params[0].nullable, None);
    }

    #[test]
    fn mixed_nullable_params() {
        let out = lex("SELECT * FROM t WHERE id = $id AND age = $age? AND name = $name").unwrap();
        assert_eq!(
            out.sql,
            "SELECT * FROM t WHERE id = $1 AND age = $2 AND name = $3"
        );
        assert_eq!(out.params.len(), 3);
        assert_eq!(out.params[0].name, "id");
        assert_eq!(out.params[0].nullable, None);
        assert_eq!(out.params[1].name, "age");
        assert_eq!(out.params[1].nullable, Some(true));
        assert_eq!(out.params[2].name, "name");
        assert_eq!(out.params[2].nullable, None);
    }

    #[test]
    fn mixed_all_three_annotations() {
        let out = lex("SELECT * FROM t WHERE a = $a AND b = $b? AND c = $c!").unwrap();
        assert_eq!(
            out.sql,
            "SELECT * FROM t WHERE a = $1 AND b = $2 AND c = $3"
        );
        assert_eq!(out.params.len(), 3);
        assert_eq!(out.params[0].nullable, None); // $a — auto
        assert_eq!(out.params[1].nullable, Some(true)); // $b? — force nullable
        assert_eq!(out.params[2].nullable, Some(false)); // $c! — force non-null
    }

    #[test]
    fn nullable_param_deduplicated() {
        let out = lex("SELECT * FROM t WHERE a = $x? OR b = $x?").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE a = $1 OR b = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "x");
        assert_eq!(out.params[0].nullable, Some(true));
    }

    #[test]
    fn spread_with_nullable_fields() {
        let out = lex("INSERT INTO t VALUES $..items { name, email?, age }").unwrap();
        assert_eq!(out.spreads.len(), 1);
        let fields = out.spreads[0].fields.as_ref().unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].name, "name");
        assert!(!fields[0].nullable);
        assert_eq!(fields[1].name, "email");
        assert!(fields[1].nullable);
        assert_eq!(fields[2].name, "age");
        assert!(!fields[2].nullable);
    }

    #[test]
    fn spread_all_nullable_fields() {
        let out = lex("INSERT INTO t VALUES $..items { a?, b? }").unwrap();
        let fields = out.spreads[0].fields.as_ref().unwrap();
        assert_eq!(fields.len(), 2);
        assert!(fields[0].nullable);
        assert!(fields[1].nullable);
    }

    #[test]
    fn rewrites_translate_named_param_offset() {
        // `$name` (5 chars) becomes `$1` (2 chars) — delta is +3 on offsets
        // after the rewrite when going post-lex -> original.
        let out = lex("SELECT * FROM users WHERE id = $name AND age > 0").unwrap();
        assert_eq!(out.sql, "SELECT * FROM users WHERE id = $1 AND age > 0");
        assert_eq!(out.rewrites.len(), 1);

        // Offset 0 (before rewrite): unchanged.
        assert_eq!(out.original_offset(0), 0);
        // Offset of "FROM" — also before the rewrite.
        let from_post = out.sql.find("FROM").unwrap();
        assert_eq!(
            out.original_offset(from_post),
            "SELECT * FROM users WHERE id = ".find("FROM").unwrap()
        );
        // Offset of "AND" — *after* the rewrite; should map back to the
        // matching position in the original string.
        let and_post = out.sql.find(" AND ").unwrap();
        let and_orig = "SELECT * FROM users WHERE id = $name AND age > 0"
            .find(" AND ")
            .unwrap();
        assert_eq!(out.original_offset(and_post), and_orig);
    }

    #[test]
    fn rewrites_translate_spread_removal() {
        // `$..items { a }` is removed entirely from the post-lex SQL.
        let out = lex("INSERT INTO t VALUES $..items { a }").unwrap();
        // Offset after the removed token should map back to its original position.
        let post_end = out.sql.len();
        let orig_end = "INSERT INTO t VALUES $..items { a }".len();
        assert_eq!(out.original_offset(post_end), orig_end);
    }

    #[test]
    fn rewrites_translate_multiple_params() {
        // Multiple $name rewrites accumulate.
        let out = lex("SELECT $first, $second_one, FROM_BIG_TABLE WHERE x = 1").unwrap();
        // Find offset of "FROM_BIG_TABLE" in both forms.
        let post = out.sql.find("FROM_BIG_TABLE").unwrap();
        let orig = "SELECT $first, $second_one, FROM_BIG_TABLE WHERE x = 1"
            .find("FROM_BIG_TABLE")
            .unwrap();
        assert_eq!(out.original_offset(post), orig);
    }

    // ---- Comments ----
    //
    // PostgreSQL has two comment forms: line comments (`-- ...` to end of line)
    // and block comments (`/* ... */`), and — unlike standard SQL — block
    // comments *nest*. The lexer must (a) ignore `$name`/`$..` inside any
    // comment, (b) preserve the comment text verbatim in the output SQL (so
    // `typedpg_pg_query` sees an equivalent statement), and (c) not treat comment
    // markers as comments when they appear inside strings, dollar-quotes, or
    // quoted identifiers (and vice-versa).

    #[test]
    fn line_comment_preserved_in_output() {
        // No params: the comment text survives verbatim in the rewritten SQL.
        let out = lex("SELECT 1 -- hello world\nFROM t").unwrap();
        assert_eq!(out.sql, "SELECT 1 -- hello world\nFROM t");
        assert!(out.params.is_empty());
    }

    #[test]
    fn line_comment_at_eof_without_newline() {
        // A line comment that runs to EOF (no closing newline) is valid, not an
        // unclosed-token error.
        let out = lex("SELECT 1 -- trailing comment").unwrap();
        assert_eq!(out.sql, "SELECT 1 -- trailing comment");
        assert!(out.params.is_empty());
    }

    #[test]
    fn line_comment_ends_at_newline() {
        // The newline closes the comment; a param on the next line is live.
        let out = lex("SELECT 1 -- $ignored\n WHERE id = $id").unwrap();
        assert_eq!(out.sql, "SELECT 1 -- $ignored\n WHERE id = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn comment_at_start_of_query() {
        let out = lex("-- leading comment\nSELECT $id").unwrap();
        assert_eq!(out.sql, "-- leading comment\nSELECT $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn multiple_line_comments() {
        let out = lex("SELECT 1 -- $a\n-- $b\nWHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn block_comment_preserved_in_output() {
        let out = lex("SELECT /* hello */ 1").unwrap();
        assert_eq!(out.sql, "SELECT /* hello */ 1");
        assert!(out.params.is_empty());
    }

    #[test]
    fn block_comment_multiline() {
        let out = lex("SELECT /* line one\n   line two */ 1 WHERE id = $id").unwrap();
        assert_eq!(
            out.sql,
            "SELECT /* line one\n   line two */ 1 WHERE id = $1"
        );
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn block_comment_between_tokens() {
        // No surrounding whitespace required.
        let out = lex("SELECT/* c */1").unwrap();
        assert_eq!(out.sql, "SELECT/* c */1");
        assert!(out.params.is_empty());
    }

    #[test]
    fn nested_block_comment() {
        let out = lex("SELECT /* a /* b */ c */ 1 WHERE id = $id").unwrap();
        assert_eq!(out.sql, "SELECT /* a /* b */ c */ 1 WHERE id = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn deeply_nested_block_comment() {
        let out = lex("SELECT /* 1 /* 2 /* 3 */ 2 */ 1 */ $id").unwrap();
        assert_eq!(out.sql, "SELECT /* 1 /* 2 /* 3 */ 2 */ 1 */ $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_nested_block_comment_ignored() {
        let out = lex("SELECT /* outer /* $inner */ $middle */ $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn unclosed_nested_block_comment_error() {
        // Inner `*/` closes one level, leaving the outer `/*` open.
        let err = lex("SELECT /* outer /* inner */").unwrap_err();
        assert!(matches!(err, LexError::UnclosedBlockComment { .. }));
    }

    #[test]
    fn spread_in_line_comment_ignored() {
        let out = lex("INSERT INTO t -- $..items\nVALUES ($id)").unwrap();
        assert!(out.spreads.is_empty());
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn spread_in_block_comment_ignored() {
        let out = lex("INSERT INTO t /* $..items { a, b } */ VALUES ($id)").unwrap();
        assert!(out.spreads.is_empty());
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    // ---- Comment markers that should NOT start a comment ----

    #[test]
    fn block_open_inside_line_comment_is_not_a_block() {
        // `/*` inside a line comment is plain text; the newline still closes the
        // line comment, and there is no dangling open block at EOF.
        let out = lex("SELECT 1 -- /* not a block\nWHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn line_marker_inside_block_comment_is_ignored() {
        // `--` inside a block comment does not change anything; `*/` still closes.
        let out = lex("SELECT /* -- $x still in block */ 1 WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn line_marker_inside_string_is_not_a_comment() {
        let out = lex("SELECT '-- not a comment $x' WHERE id = $id").unwrap();
        assert_eq!(out.sql, "SELECT '-- not a comment $x' WHERE id = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn block_marker_inside_string_is_not_a_comment() {
        let out = lex("SELECT '/* not a comment $x */' WHERE id = $id").unwrap();
        assert_eq!(out.sql, "SELECT '/* not a comment $x */' WHERE id = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn line_marker_inside_dollar_quote_is_not_a_comment() {
        let out = lex("SELECT $$ -- $x not a comment $$ WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn block_marker_inside_dollar_quote_is_not_a_comment() {
        let out = lex("SELECT $$ /* $x not a comment */ $$ WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn comment_markers_inside_quoted_identifier_are_not_comments() {
        let out = lex("SELECT \"-- /* weird col */\" FROM t WHERE id = $id").unwrap();
        assert_eq!(
            out.sql,
            "SELECT \"-- /* weird col */\" FROM t WHERE id = $1"
        );
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn dollar_quote_marker_inside_line_comment_is_inert() {
        // `$$` inside a line comment must not open a dollar-quote; the newline
        // closes the comment and the following param stays live.
        let out = lex("SELECT 1 -- $$ not a dollar quote\nWHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn string_marker_inside_block_comment_is_inert() {
        // A lone `'` inside a block comment must not open a string literal — the
        // `*/` still closes the comment and the trailing param stays live.
        let out = lex("SELECT /* it's fine $x */ 1 WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }
}
