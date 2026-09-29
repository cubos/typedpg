//! Parsing and orchestration for the `sql!` proc macro.
//!
//! Parses the macro input, builds a cached [`PgCatalog`] from migrations,
//! runs static analysis, and generates typed Rust code.

use std::cell::RefCell;
use std::path::Path;

use proc_macro2::Span;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, LitStr, Token};
use typedpg_analyzer::PgCatalog;

use crate::codegen::{self, ParamAssignment};

// ---------------------------------------------------------------------------
// PgCatalog caching
// ---------------------------------------------------------------------------

struct CachedPgCatalog {
    catalog: PgCatalog,
    /// Cache key: migration hash.
    migration_hash: String,
}

thread_local! {
    static CACHED_PG_CATALOG: RefCell<Option<CachedPgCatalog>> = const { RefCell::new(None) };
}

/// Build (or retrieve from cache) a [`PgCatalog`] from migration files.
fn get_or_build_pg_catalog(
    migrations_dirs: &[&Path],
    migration_hash: &str,
) -> Result<PgCatalog, syn::Error> {
    CACHED_PG_CATALOG.with(|cell| {
        let borrow = cell.borrow();
        if let Some(cached) = borrow.as_ref()
            && cached.migration_hash == migration_hash
        {
            return Ok(cached.catalog.clone());
        }
        drop(borrow);

        let migrations = collect_migration_files(migrations_dirs)?;
        let mut catalog = PgCatalog::new().map_err(|e| {
            syn::Error::new(
                Span::call_site(),
                format!("failed to load embedded PG catalog seed: {e}"),
            )
        })?;
        for (filename, sql) in &migrations {
            catalog.apply_sql(sql).map_err(|e| {
                syn::Error::new(
                    Span::call_site(),
                    format!("DDL interpretation failed in '{filename}': {e}"),
                )
            })?;
        }

        cell.borrow_mut().replace(CachedPgCatalog {
            catalog: catalog.clone(),
            migration_hash: migration_hash.to_string(),
        });

        Ok(catalog)
    })
}

/// Collect all migration SQL files from the given directories.
///
/// Returns `(filename, content)` pairs sorted by filename.
fn collect_migration_files(dirs: &[&Path]) -> Result<Vec<(String, String)>, syn::Error> {
    let mut files = Vec::new();

    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let entries = std::fs::read_dir(dir).map_err(|e| {
            syn::Error::new(
                Span::call_site(),
                format!("failed to read migrations dir '{}': {e}", dir.display()),
            )
        })?;
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.to_string_lossy().to_string();
            if name.ends_with(".sql") && !name.ends_with(".down.sql") {
                let filename = entry.file_name().to_string_lossy().to_string();
                let content = std::fs::read_to_string(&path).map_err(|e| {
                    syn::Error::new(
                        Span::call_site(),
                        format!("failed to read migration '{}': {e}", path.display()),
                    )
                })?;
                files.push((filename, content));
            }
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

// ---------------------------------------------------------------------------
// Macro input parsing
// ---------------------------------------------------------------------------

/// Parsed representation of `sql!([db = name,] executor, "SQL", param = value, ...)`.
pub struct QueryInput {
    pub db_name: Option<Ident>,
    pub executor: Expr,
    pub sql: LitStr,
    pub assignments: Vec<ParamAssignment>,
}

impl Parse for QueryInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        // Check for optional `db = <name>,` prefix.
        let db_name = if input.peek(Ident) {
            let fork = input.fork();
            let ident: Ident = fork.parse()?;
            if ident == "db" && fork.peek(Token![=]) {
                // Consume from the real stream.
                let _: Ident = input.parse()?;
                input.parse::<Token![=]>()?;
                let name: Ident = input.parse()?;
                input.parse::<Token![,]>()?;
                Some(name)
            } else {
                None
            }
        } else {
            None
        };

        let executor: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let sql: LitStr = input.parse()?;

        let mut assignments = Vec::new();
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let name: Ident = input.parse()?;
            if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                let expr: Expr = input.parse()?;
                assignments.push(ParamAssignment {
                    name: name.to_string(),
                    expr: Some(expr),
                });
            } else {
                assignments.push(ParamAssignment {
                    name: name.to_string(),
                    expr: None,
                });
            }
        }

        Ok(Self {
            db_name,
            executor,
            sql,
            assignments,
        })
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Parse a Rust string literal preserving line continuations as newlines.
///
/// Rust's normal string processing collapses `\<newline><leading_ws>` to
/// nothing — so a SQL literal written across multiple source lines arrives
/// at the analyzer as one long line, and diagnostic snippets can't show
/// the user's original layout. Here we keep the line break (emitting a
/// real `\n`) and let the leading whitespace flow through, so the snippet
/// renders the SQL the way it was written.
///
/// All other Rust escapes (`\n`, `\t`, `\x{..}`, `\u{..}`, …) follow the
/// usual rules. Raw strings (`r"…"`, `r#"…"#`) pass through unchanged
/// because they have no escapes to begin with.
fn parse_sql_literal_preserving_linebreaks(lit: &LitStr) -> String {
    let raw = lit.token().to_string();
    let bytes = raw.as_bytes();
    // Detect a raw-string prefix (`r"…"` or `r#…"…"#…`). Those have no
    // escapes — fall through to LitStr::value().
    if bytes.first() == Some(&b'r') {
        return lit.value();
    }
    // Regular string: strip surrounding `"` and process escapes.
    let inner = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"'));
    let Some(inner) = inner else {
        return lit.value();
    };

    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\n') => {
                // Line continuation. Rust would discard the newline and any
                // leading whitespace on the next line; we keep the newline
                // and let the whitespace flow so the original layout
                // survives in the SQL.
                out.push('\n');
            }
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some('0') => out.push('\0'),
            Some('x') => {
                let hi = chars.next().unwrap_or('0');
                let lo = chars.next().unwrap_or('0');
                let h: String = [hi, lo].iter().collect();
                if let Ok(n) = u8::from_str_radix(&h, 16) {
                    out.push(n as char);
                }
            }
            Some('u') if chars.peek() == Some(&'{') => {
                chars.next();
                let mut hex = String::new();
                for p in chars.by_ref() {
                    if p == '}' {
                        break;
                    }
                    hex.push(p);
                }
                if let Ok(cp) = u32::from_str_radix(&hex, 16)
                    && let Some(c) = char::from_u32(cp)
                {
                    out.push(c);
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Execute the full `sql!` pipeline and return the generated `TokenStream`.
pub fn expand(input: QueryInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    let sql_str = parse_sql_literal_preserving_linebreaks(&input.sql);

    // 1. Load project config from Cargo.toml.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| {
        syn::Error::new(
            Span::call_site(),
            "CARGO_MANIFEST_DIR not set — are you running inside cargo build?",
        )
    })?;
    let manifest_path = Path::new(&manifest_dir);
    let cargo_toml_path = manifest_path.join("Cargo.toml");
    let config = typedpg_core::config::Config::from_cargo_toml(&cargo_toml_path)
        .map_err(|e| syn::Error::new(Span::call_site(), format!("failed to load config: {e}")))?;

    let db_name_str = input.db_name.as_ref().map(|i| i.to_string());
    let resolved = config.resolve(db_name_str.as_deref()).map_err(|e| {
        syn::Error::new(
            input
                .db_name
                .as_ref()
                .map(|i| i.span())
                .unwrap_or(Span::call_site()),
            e.to_string(),
        )
    })?;

    // 2. Build (or reuse cached) PgCatalog from migrations.
    let migrations_dir = resolved.migrations_dir(manifest_path);
    let extra_dirs = resolved.extra_migrations_dirs(manifest_path);
    let mut all_dirs: Vec<&Path> = vec![migrations_dir.as_path()];
    all_dirs.extend(extra_dirs.iter().map(|p| p.as_path()));

    let migration_hash = crate::migrations_hash::hash_migrations_dirs(&all_dirs).map_err(|e| {
        syn::Error::new(Span::call_site(), format!("failed to hash migrations: {e}"))
    })?;

    let catalog = get_or_build_pg_catalog(&all_dirs, &migration_hash)?;

    // 3. Analyze the SQL (lex + type inference in one pass).
    let analyzed = catalog
        .analyze(&sql_str)
        .map_err(|e| syn::Error::new(input.sql.span(), e.to_string()))?;

    // A native `$1` placeholder is a valid PG parameter, but it has no name
    // an argument could bind to.
    if let Some(p) = analyzed
        .params
        .iter()
        .find(|p| p.name.starts_with(|c: char| c.is_ascii_digit()))
    {
        return Err(syn::Error::new(
            input.sql.span(),
            format!(
                "positional placeholder `${}` is not supported in sql!: name the parameter \
                 (e.g. `$id`) so it can be bound",
                p.name
            ),
        ));
    }

    // 4. Validate that all assignments match SQL params/spreads.
    for assignment in &input.assignments {
        if !analyzed.params.iter().any(|p| p.name == assignment.name)
            && !analyzed.spreads.iter().any(|s| s.name == assignment.name)
        {
            let available: Vec<String> = analyzed
                .params
                .iter()
                .map(|p| format!("${}", p.name))
                .chain(analyzed.spreads.iter().map(|s| format!("$..{}", s.name)))
                .collect();
            let available_str = if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            };
            return Err(syn::Error::new(
                input.sql.span(),
                format!(
                    "unknown parameter `{}` — not found in SQL. Available parameters: {}",
                    assignment.name, available_str,
                ),
            ));
        }
    }

    // 5. Validate spread constraints: field names must be unique within each spread.
    for spread in &analyzed.spreads {
        let mut seen = std::collections::HashSet::new();
        for field in &spread.fields {
            if !seen.insert(field.name.as_str()) {
                return Err(syn::Error::new(
                    input.sql.span(),
                    format!(
                        "duplicate field '{}' in $..{} spread",
                        field.name, spread.name,
                    ),
                ));
            }
        }
    }

    // 6. Generate typed Rust code.
    codegen::generate(&analyzed, &resolved, &input.executor, &input.assignments)
}

#[cfg(test)]
mod tests {
    use super::parse_sql_literal_preserving_linebreaks;
    use syn::LitStr;

    /// Parse a Rust source fragment as a string literal so tests can express
    /// the *exact* token text — including backslash-newline continuations.
    fn lit(src: &str) -> LitStr {
        syn::parse_str::<LitStr>(src).expect("test input must be a valid Rust string literal")
    }

    #[test]
    fn plain_string_passes_through() {
        let l = lit("\"SELECT 1\"");
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "SELECT 1");
    }

    #[test]
    fn line_continuation_preserved_as_newline() {
        // Source: "foo \\\n   bar"  →  LitStr::value() would give "foo bar"
        //                                we want "foo \n   bar"
        let l = lit("\"foo \\\n   bar\"");
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "foo \n   bar");
    }

    #[test]
    fn multiple_continuations_all_preserved() {
        let l = lit("\"a \\\n b \\\n c\"");
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "a \n b \n c");
    }

    #[test]
    fn continuation_keeps_indentation() {
        // Realistic SQL layout: each line continues with eight spaces.
        let l = lit("\"SELECT id \\\n        FROM users\"");
        assert_eq!(
            parse_sql_literal_preserving_linebreaks(&l),
            "SELECT id \n        FROM users",
        );
    }

    #[test]
    fn standard_escape_n_still_works() {
        let l = lit("\"a\\nb\"");
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "a\nb");
    }

    #[test]
    fn standard_escape_t_still_works() {
        let l = lit("\"a\\tb\"");
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "a\tb");
    }

    #[test]
    fn escape_backslash() {
        let l = lit(r#""a\\b""#);
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "a\\b");
    }

    #[test]
    fn escape_double_quote() {
        let l = lit(r#""he said \"hi\"""#);
        assert_eq!(
            parse_sql_literal_preserving_linebreaks(&l),
            "he said \"hi\"",
        );
    }

    #[test]
    fn escape_single_quote() {
        let l = lit(r#""it\'s ok""#);
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "it's ok");
    }

    #[test]
    fn escape_null_byte() {
        let l = lit(r#""a\0b""#);
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "a\0b");
    }

    #[test]
    fn escape_hex_byte() {
        // \x41 = 'A'
        let l = lit(r#""\x41BC""#);
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "ABC");
    }

    #[test]
    fn escape_unicode() {
        // \u{4E2D} = '中'
        let l = lit(r#""\u{4E2D}""#);
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "中");
    }

    #[test]
    fn escape_carriage_return() {
        let l = lit(r#""a\rb""#);
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "a\rb");
    }

    #[test]
    fn raw_string_passes_through_lit_value() {
        // Raw strings have no escapes; backslashes are literal. The
        // function detects this and falls back to LitStr::value().
        let l = lit(r##"r"a\nb""##);
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "a\\nb");
    }

    #[test]
    fn raw_string_with_hashes_passes_through() {
        let l = lit(r###"r#"with "quotes" inside"#"###);
        assert_eq!(
            parse_sql_literal_preserving_linebreaks(&l),
            r#"with "quotes" inside"#,
        );
    }

    #[test]
    fn continuation_at_start_of_line() {
        // Continuation right after the opening quote.
        let l = lit("\"\\\n  hello\"");
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "\n  hello");
    }

    #[test]
    fn continuation_mixed_with_escape_n() {
        // Continuation + an explicit \n on the same line.
        let l = lit("\"a\\nb \\\n  c\"");
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "a\nb \n  c");
    }

    #[test]
    fn empty_string() {
        let l = lit("\"\"");
        assert_eq!(parse_sql_literal_preserving_linebreaks(&l), "");
    }

    #[test]
    fn realistic_multiline_sql_matches_visual_layout() {
        // The kind of literal that originally collapsed onto a single
        // line in the diagnostic snippet (the bug this function fixes).
        let l = lit("\"SELECT id, name \\\n   FROM users \\\n  WHERE id = $id\"");
        assert_eq!(
            parse_sql_literal_preserving_linebreaks(&l),
            "SELECT id, name \n   FROM users \n  WHERE id = $id",
        );
    }
}
