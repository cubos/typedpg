//! `embed_migrations!("path")` — bake a `MigrationSource` from a directory
//! of `NNNN_description.sql` files into the binary at compile time.
//!
//! Walks the directory at macro-expansion time, validates the filename
//! format the same way `MigrationSource::from_dir` does, and emits a call
//! to `MigrationSource::from_embedded` populated with `include_str!`
//! references — so each migration's contents land in the binary as a
//! `&'static str` and changes to the files re-trigger a rebuild via
//! `include_str!`'s tracking.

use std::collections::HashMap;
use std::path::PathBuf;

use proc_macro2::TokenStream;
use quote::quote;
use syn::{LitStr, parse::Parse};

pub struct EmbedInput {
    path: LitStr,
}

impl Parse for EmbedInput {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        Ok(Self {
            path: input.parse()?,
        })
    }
}

pub fn expand(input: EmbedInput) -> syn::Result<TokenStream> {
    let span = input.path.span();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| {
        syn::Error::new(
            span,
            "CARGO_MANIFEST_DIR is not set — embed_migrations! must run under cargo",
        )
    })?;

    // Resolve the user-supplied path (e.g. "./migrations") relative to the
    // crate that's invoking the macro, mirroring how `include_str!` works.
    let raw_path = input.path.value();
    let abs_path = PathBuf::from(&manifest_dir).join(&raw_path);
    if !abs_path.is_dir() {
        return Err(syn::Error::new(
            span,
            format!(
                "embed_migrations! path is not a directory: {} (resolved to {})",
                raw_path,
                abs_path.display(),
            ),
        ));
    }

    let entries = std::fs::read_dir(&abs_path).map_err(|e| {
        syn::Error::new(
            span,
            format!("failed to read directory {}: {e}", abs_path.display()),
        )
    })?;

    // Two passes mirroring MigrationSource::from_dir: collect down files
    // by base name first, then attach to up files.
    let mut down_files: HashMap<String, PathBuf> = HashMap::new();
    let mut up_files: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| syn::Error::new(span, e.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(base) = file_name.strip_suffix(".down.sql") {
            down_files.insert(base.to_owned(), path);
        } else if let Some(stem) = file_name.strip_suffix(".sql") {
            up_files.push((stem.to_owned(), path));
        }
    }

    // Validate each stem follows NNNN_description and sort by version. We
    // reproduce the validator inline (rather than calling into the runtime
    // crate) so the macro stays free of runtime deps.
    let mut entries: Vec<(String, PathBuf, Option<PathBuf>)> = Vec::with_capacity(up_files.len());
    for (stem, up_path) in up_files {
        validate_stem(&stem, &up_path).map_err(|msg| syn::Error::new(span, msg))?;
        let down_path = down_files.remove(&stem);
        entries.push((stem, up_path, down_path));
    }
    entries.sort_by(|a, b| by_version(&a.0, &b.0));

    // Generate the call to MigrationSource::from_embedded. Each up/down
    // path goes through include_str!, so cargo re-runs the macro when any
    // SQL file changes.
    let tuples = entries.iter().map(|(stem, up, down)| {
        let stem_lit = LitStr::new(stem, span);
        let up_lit = LitStr::new(&up.to_string_lossy(), span);
        let down_tokens = match down {
            Some(d) => {
                let d_lit = LitStr::new(&d.to_string_lossy(), span);
                quote! { ::core::option::Option::Some(include_str!(#d_lit)) }
            }
            None => quote! { ::core::option::Option::None },
        };
        quote! { ( #stem_lit, include_str!(#up_lit), #down_tokens ) }
    });

    Ok(quote! {
        ::typedpg::migrate::MigrationSource::from_embedded([
            #(#tuples),*
        ])
        .expect("embed_migrations! produced an invalid MigrationSource (this is a bug)")
    })
}

/// Migrations are ordered by their numeric prefix: `2` before `10`. The
/// prefix is a number, not text — `10_x` is a later migration than `2_x`.
fn by_version(a: &str, b: &str) -> std::cmp::Ordering {
    let version = |stem: &str| {
        stem.split_once('_')
            .and_then(|(prefix, _)| prefix.parse::<u128>().ok())
    };
    match (version(a), version(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

/// Inline copy of `MigrationSource::from_dir`'s stem validator. Returning
/// the same error wording keeps the user experience consistent across the
/// runtime and macro paths.
fn validate_stem(stem: &str, path: &std::path::Path) -> Result<(), String> {
    let underscore_pos = stem.find('_').ok_or_else(|| {
        format!(
            "migration file does not follow NNNN_description.sql format: {}",
            path.display()
        )
    })?;
    let prefix = &stem[..underscore_pos];
    let description = &stem[underscore_pos + 1..];
    if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "migration file does not have a numeric prefix (NNNN_...): {}",
            path.display()
        ));
    }
    if description.is_empty() {
        return Err(format!(
            "migration file has no description after prefix: {}",
            path.display()
        ));
    }
    Ok(())
}
