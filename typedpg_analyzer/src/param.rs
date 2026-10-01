//! Data structures representing parameters extracted from SQL templates.
//!
//! These types are produced by the [lexer](crate::lexer) and consumed by the
//! proc macro during code generation. They are not typically used directly by
//! end users of the `typedpg` library.

/// A named parameter extracted from a SQL template (e.g., `$user_id`).
///
/// Parameters are deduplicated: each unique name appears once, in order of
/// first appearance. The index in the `params` vec (+1) is the positional
/// placeholder used in the rewritten SQL (i.e., `params[0]` becomes `$1`).
///
/// Nullability annotation: `$foo` has no annotation (inferred from schema),
/// `$foo?` forces nullable, `$foo!` forces non-nullable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Param {
    /// The parameter name without the `$` prefix and `?`/`!` suffix (e.g., `"user_id"`).
    pub name: String,
    /// Explicit nullability annotation.
    /// - `None`: no annotation — nullability will be inferred from schema context.
    /// - `Some(true)`: `$foo?` — force nullable (`Option<T>`).
    /// - `Some(false)`: `$foo!` — force non-nullable.
    pub nullable: Option<bool>,
    /// Byte offsets in the output SQL immediately after each `$N` placeholder
    /// for this parameter. Used by codegen to insert type casts (e.g., `::jsonb`).
    /// A param referenced multiple times will have multiple offsets.
    pub sql_offsets: Vec<usize>,
}

/// A field inside a spread parameter, with optional nullable annotation.
///
/// `email?` in `$..items { name, email?, age }` produces
/// `SpreadField { name: "email", nullable: true }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpreadField {
    /// Field name without the `?` suffix.
    pub name: String,
    /// Whether this field is nullable (`field?` syntax).
    pub nullable: bool,
}

impl From<&str> for SpreadField {
    fn from(s: &str) -> Self {
        SpreadField {
            name: s.to_string(),
            nullable: false,
        }
    }
}

/// A spread parameter extracted from a SQL template.
///
/// Spread parameters use the `$..name` or `$..name { field1, field2? }` syntax
/// and are used for bulk inserts. The spread token is removed from the output
/// SQL, and the [`offset`](SpreadParam::offset) field marks the byte position
/// where the proc macro will insert the expanded positional placeholders
/// (e.g., `($1,$2,$3),($4,$5,$6)`).
///
/// Positional indices for spread placeholders start after the last named
/// parameter index (`params.len() + 1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpreadParam {
    /// The spread name without the `$..` prefix (e.g., `"items"`).
    pub name: String,
    /// Explicit fields when provided inline (e.g., `{ name, email?, age }`),
    /// or `None` if the spread was written without a field list.
    pub fields: Option<Vec<SpreadField>>,
    /// Byte offset in the output SQL where the expanded placeholders should be
    /// inserted by the code generator.
    pub offset: usize,
}

/// A token the lexer rewrote (or removed) while producing the post-lex SQL.
///
/// Used to translate byte offsets from the post-lex SQL — which is what the
/// parser sees and what AST nodes' `location` field refers to — back to the
/// original SQL the user wrote. That mapping is what lets diagnostics
/// pinpoint the offending text inside the Rust `sql!` literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rewrite {
    /// Byte offset in the **original** SQL where the rewritten token begins.
    pub original_at: usize,
    /// Byte offset in the **post-lex** SQL where the rewritten token begins.
    pub post_lex_at: usize,
    /// Length in bytes the token had in the original SQL.
    pub original_len: usize,
    /// Length in bytes the token has in the post-lex SQL (0 when removed,
    /// e.g. a `$..spread`).
    pub post_lex_len: usize,
}

/// The result of lexing a SQL template via [`crate::lexer::lex`].
///
/// Contains the rewritten SQL (with positional placeholders), the list of
/// deduplicated named parameters, and any spread parameters found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LexOutput {
    /// SQL with named params rewritten to positional placeholders (`$1`, `$2`, ...).
    ///
    /// Spread tokens are removed from this string. The caller must insert
    /// expanded placeholders at each spread's [`offset`](SpreadParam::offset).
    pub sql: String,
    /// Unique named parameters in order of first appearance.
    ///
    /// `params[0]` corresponds to `$1`, `params[1]` to `$2`, etc.
    pub params: Vec<Param>,
    /// Spread parameters in order of appearance in the SQL.
    pub spreads: Vec<SpreadParam>,
    /// Rewrites applied by the lexer, in post-lex order. Used to translate
    /// AST `location` offsets (post-lex) back into the original SQL for
    /// diagnostics.
    pub rewrites: Vec<Rewrite>,
}

impl LexOutput {
    /// The output of lexing `sql` with nothing to rewrite: offsets map to
    /// themselves. For SQL that isn't a `sql!` template (a migration).
    pub(crate) fn identity(sql: &str) -> Self {
        Self {
            sql: sql.to_owned(),
            params: Vec::new(),
            spreads: Vec::new(),
            rewrites: Vec::new(),
        }
    }

    /// Translate a byte offset from the post-lex SQL back into the original
    /// SQL. If `post_lex` falls inside a removed/rewritten token, returns the
    /// start of that token in the original SQL.
    pub(crate) fn original_offset(&self, post_lex: usize) -> usize {
        let mut original = post_lex;
        for rw in &self.rewrites {
            let rw_end_post_lex = rw.post_lex_at + rw.post_lex_len;
            if rw_end_post_lex <= post_lex {
                // Rewrite is entirely before `post_lex` — apply its delta.
                let delta = rw.original_len as isize - rw.post_lex_len as isize;
                original = (original as isize + delta) as usize;
            } else if rw.post_lex_at <= post_lex {
                // `post_lex` falls inside the rewritten token. Map to the
                // start of the original token.
                return rw.original_at;
            } else {
                // Past the relevant rewrites (they are stored in order).
                break;
            }
        }
        original
    }

    /// Translate a byte span from the post-lex SQL back into the original SQL.
    pub(crate) fn original_span(&self, start: usize, end: usize) -> (usize, usize) {
        (self.original_offset(start), self.original_offset(end))
    }
}
