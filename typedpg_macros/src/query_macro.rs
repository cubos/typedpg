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
    /// The catalog, or how building it failed: a broken migration is
    /// reported in full by the first `sql!` that builds the catalog, and
    /// in one line by the others (see [`get_or_build_pg_catalog`]).
    catalog: Result<PgCatalog, CatalogFailure>,
    /// Cache key: migration hash.
    migration_hash: String,
    /// Cache key: the runner's `use_transaction` setting.
    use_transaction: bool,
}

thread_local! {
    static CACHED_PG_CATALOG: RefCell<Option<CachedPgCatalog>> = const { RefCell::new(None) };
}

/// A migration the catalog could not be built past.
#[derive(Clone)]
struct CatalogFailure {
    /// The full diagnostic: the statement's message, `--> file:line:col`,
    /// the snippet.
    full: String,
    /// The message's first line, for the one-line repeat.
    summary: String,
    /// The migration's path, as shown in the diagnostic.
    filename: String,
}

/// Build (or retrieve from cache) a [`PgCatalog`] from migration files.
/// `use_transaction` is the migration runner's setting: whether it wraps
/// each migration in a transaction.
///
/// A migration that fails fails every `sql!` of the crate. The first one
/// to build the catalog reports the full diagnostic; the cached failure
/// makes the others report a one-line summary — the same snippet at every
/// query of the crate would bury everything else.
fn get_or_build_pg_catalog(
    migrations_dirs: &[&Path],
    migration_hash: &str,
    use_transaction: bool,
) -> Result<PgCatalog, syn::Error> {
    CACHED_PG_CATALOG.with(|cell| {
        let borrow = cell.borrow();
        if let Some(cached) = borrow.as_ref()
            && cached.migration_hash == migration_hash
            && cached.use_transaction == use_transaction
        {
            return match &cached.catalog {
                Ok(catalog) => Ok(catalog.clone()),
                Err(failure) => Err(syn::Error::new(
                    Span::call_site(),
                    format!(
                        "migration {} failed: {} (reported in full at the first sql! of this \
                         crate)",
                        failure.filename, failure.summary
                    ),
                )),
            };
        }
        drop(borrow);

        let migrations = collect_migration_files(migrations_dirs)?;
        let mut catalog = PgCatalog::new().map_err(|e| {
            syn::Error::new(
                Span::call_site(),
                format!("failed to load embedded PG catalog seed: {e}"),
            )
        })?;
        catalog.set_migrations_use_transaction(use_transaction);
        let mut result = Ok(());
        for (filename, sql) in &migrations {
            if let Err(e) = catalog.apply_migration(filename, sql) {
                let full = e.to_string();
                let summary = full.lines().next().unwrap_or_default().to_owned();
                result = Err(CatalogFailure {
                    full,
                    summary,
                    filename: filename.clone(),
                });
                break;
            }
        }
        let built = result.map(|()| catalog);

        cell.borrow_mut().replace(CachedPgCatalog {
            catalog: built.clone(),
            migration_hash: migration_hash.to_string(),
            use_transaction,
        });

        built.map_err(|failure| syn::Error::new(Span::call_site(), failure.full))
    })
}

/// Collect all migration SQL files from the given directories.
///
/// Returns `(path, content)` pairs sorted by file name (whatever the
/// directory), `path` in the form diagnostics show.
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
                // Shown in diagnostics (`--> migrations/0002_more.sql:9:12`):
                // the path from the crate root when it is under it, as
                // rustc's own paths are.
                let filename = display_path(&path);
                let content = std::fs::read_to_string(&path).map_err(|e| {
                    syn::Error::new(
                        Span::call_site(),
                        format!("failed to read migration '{}': {e}", path.display()),
                    )
                })?;
                files.push((entry.file_name(), filename, content));
            }
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files
        .into_iter()
        .map(|(_, path, sql)| (path, sql))
        .collect())
}

/// `path` relative to the crate being compiled when it is under it — the
/// form rustc prints its own source paths in — else as configured.
fn display_path(path: &Path) -> String {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let shown = path
        .strip_prefix(&manifest_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    shown.strip_prefix("./").map(str::to_owned).unwrap_or(shown)
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
/// The project's `[package.metadata.typedpg]` config.
pub(crate) fn load_config() -> Result<typedpg_core::config::Config, syn::Error> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| {
        syn::Error::new(
            Span::call_site(),
            "CARGO_MANIFEST_DIR not set — are you running inside cargo build?",
        )
    })?;
    let cargo_toml_path = Path::new(&manifest_dir).join("Cargo.toml");
    typedpg_core::config::Config::from_cargo_toml(&cargo_toml_path)
        .map_err(|e| syn::Error::new(Span::call_site(), format!("failed to load config: {e}")))
}

/// The catalog a database's migrations build, and what to tell the user
/// about how it was built.
pub(crate) struct BuiltCatalog<'c> {
    pub catalog: PgCatalog,
    pub resolved: typedpg_core::config::ResolvedConfig<'c>,
    /// Set when a configured migrations directory does not exist — a
    /// missing directory counts as no migrations, so a typo in the path
    /// shows up as every table being missing. Appended to analysis errors.
    pub missing_dirs_note: Option<String>,
}

impl BuiltCatalog<'_> {
    /// `error` with the [`Self::missing_dirs_note`], if any.
    pub(crate) fn annotate(&self, error: String) -> String {
        match &self.missing_dirs_note {
            Some(note) => format!("{}\n  note: {note}", error.trim_end()),
            None => error,
        }
    }
}

/// The database `db_name` names (the default one without it) and the
/// catalog its migrations build.
pub(crate) fn catalog_for<'c>(
    config: &'c typedpg_core::config::Config,
    db_name: Option<&Ident>,
) -> Result<BuiltCatalog<'c>, syn::Error> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let manifest_path = Path::new(&manifest_dir);
    let db_name_str = db_name.map(|i| i.to_string());
    let resolved = config.resolve(db_name_str.as_deref()).map_err(|e| {
        syn::Error::new(
            db_name.map(|i| i.span()).unwrap_or(Span::call_site()),
            e.to_string(),
        )
    })?;

    // Build (or reuse cached) PgCatalog from migrations.
    let migrations_dir = resolved.migrations_dir(manifest_path);
    let extra_dirs = resolved.extra_migrations_dirs(manifest_path);
    let mut all_dirs: Vec<&Path> = vec![migrations_dir.as_path()];
    all_dirs.extend(extra_dirs.iter().map(|p| p.as_path()));

    let migration_hash = crate::migrations_hash::hash_migrations_dirs(&all_dirs).map_err(|e| {
        syn::Error::new(Span::call_site(), format!("failed to hash migrations: {e}"))
    })?;

    let catalog = get_or_build_pg_catalog(
        &all_dirs,
        &migration_hash,
        config.migrations.use_transaction,
    )?;
    let missing: Vec<String> = all_dirs
        .iter()
        .filter(|d| !d.is_dir())
        .map(|d| format!("'{}'", display_path(d)))
        .collect();
    let missing_dirs_note = (!missing.is_empty()).then(|| {
        format!(
            "no migrations were loaded from {}: the directory does not exist (see \
             [package.metadata.typedpg.database] in Cargo.toml)",
            missing.join(", ")
        )
    });
    Ok(BuiltCatalog {
        catalog,
        resolved,
        missing_dirs_note,
    })
}

pub fn expand(input: QueryInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    let sql_str = parse_sql_literal_preserving_linebreaks(&input.sql);
    let config = load_config()?;
    let built = catalog_for(&config, input.db_name.as_ref())?;

    // 3. Analyze the SQL (lex + type inference in one pass).
    let analyzed = built
        .catalog
        .analyze(&sql_str)
        .map_err(|e| syn::Error::new(input.sql.span(), built.annotate(e.to_string())))?;
    let resolved = &built.resolved;

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
    codegen::generate(&analyzed, resolved, &input.executor, &input.assignments)
}

#[cfg(test)]
mod catalog_tests {
    use super::get_or_build_pg_catalog;

    #[test]
    fn a_failing_migration_is_reported_in_full_once() {
        let dir = std::env::temp_dir().join(format!(
            "typedpg-macros-failing-migration-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0001_init.sql"), "CREATE TABLE t (id int);\n").unwrap();
        std::fs::write(
            dir.join("0002_view.sql"),
            "CREATE VIEW v AS\n    SELECT idd FROM t;\n",
        )
        .unwrap();
        let first = get_or_build_pg_catalog(&[dir.as_path()], "failing", true)
            .err()
            .expect("the migration fails")
            .to_string();
        let second = get_or_build_pg_catalog(&[dir.as_path()], "failing", true)
            .err()
            .expect("the failure is cached")
            .to_string();
        std::fs::remove_dir_all(&dir).unwrap();

        let path = dir.join("0002_view.sql").display().to_string();
        assert_eq!(
            first,
            format!(
                "column \"idd\" does not exist (while analyzing view 'public.v')
  --> {path}:2:12
  ╭────
2 │     SELECT idd FROM t;
  ·            ─┬─
  ·             ╰─ column does not exist
  ╰────
  help: Perhaps you meant to reference the column \"t.id\"."
            )
        );
        assert_eq!(
            second,
            format!(
                "migration {path} failed: column \"idd\" does not exist (while analyzing view \
                 'public.v') (reported in full at the first sql! of this crate)"
            )
        );
    }
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
