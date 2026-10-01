//! Code generation for the `sql!` macro.
//!
//! Receives an [`AnalyzedQuery`] from `typedpg_analyzer` and produces a
//! [`proc_macro2::TokenStream`] that implements the typed query builder.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse_str;

use typedpg_analyzer::{
    AnalyzedColumn, AnalyzedCopyIn, AnalyzedParam, AnalyzedQuery, AnalyzedSpreadField,
    QualifiedName, RecordField, Type,
};
use typedpg_core::config::ResolvedConfig;

use crate::pg_type_map;

/// Rust keywords that cannot be used as identifiers without the `r#` prefix.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
    "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "yield", "abstract", "become", "box", "do", "final",
    "macro", "override", "priv", "try", "typeof", "unsized", "virtual",
];

/// Sanitize a SQL column name into a valid Rust identifier.
fn make_field_ident(name: &str) -> proc_macro2::Ident {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    let sanitized = if sanitized.is_empty() {
        "_unnamed".to_string()
    } else if sanitized.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{sanitized}")
    } else {
        sanitized
    };

    if RUST_KEYWORDS.contains(&sanitized.as_str()) {
        proc_macro2::Ident::new_raw(&sanitized, proc_macro2::Span::call_site())
    } else {
        format_ident!("{}", sanitized)
    }
}

/// Wrap the SQL in a `SELECT * FROM (<sql>) AS __typedpg_limit LIMIT 2`
/// subquery so that `fetch_one` / `fetch_optional` can detect more-than-one-
/// row without fetching the entire result set.
///
/// Only safe when [`AnalyzedQuery::can_run_as_subquery`] is true. PG rejects
/// the wrap for top-level DML (`INSERT`/`UPDATE`/`DELETE`/`MERGE`, with or
/// without `RETURNING`), utility statements (`EXPLAIN`/`NOTIFY`/…), and
/// `WITH … (DML …) SELECT …` — for those we send the SQL unwrapped and
/// let the runtime materialize all returned rows before the row-count check.
fn wrap_with_limit(sql: &str, can_run_as_subquery: bool) -> Option<String> {
    can_run_as_subquery.then(|| format!("SELECT * FROM ({sql}) AS __typedpg_limit LIMIT 2"))
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A named parameter assignment from the macro invocation.
///
/// Examples:
/// - `min_age = 25`  → `ParamAssignment { name: "min_age", expr: Some(25) }`
/// - `min_age`       → `ParamAssignment { name: "min_age", expr: None }` (scope capture)
pub struct ParamAssignment {
    /// Parameter name — must match a `$name` token found in the SQL.
    pub name: String,
    /// The value expression. `None` means the variable with the same name is
    /// captured from the enclosing scope.
    pub expr: Option<syn::Expr>,
}

/// Small accessors so the same helpers work for regular params and spread fields.
trait TypedParam {
    fn pg_type(&self) -> &Type;
    fn nullable(&self) -> bool;
}

impl TypedParam for AnalyzedParam {
    fn pg_type(&self) -> &Type {
        &self.pg_type
    }
    fn nullable(&self) -> bool {
        self.nullable
    }
}

/// A `copy_in!` target column, as the value a row supplies for it.
impl TypedParam for AnalyzedColumn {
    fn pg_type(&self) -> &Type {
        &self.pg_type
    }
    fn nullable(&self) -> bool {
        self.nullable
    }
}

impl TypedParam for AnalyzedSpreadField {
    fn pg_type(&self) -> &Type {
        &self.pg_type
    }
    fn nullable(&self) -> bool {
        self.nullable
    }
}

// ---------------------------------------------------------------------------
// Record registry: synthesized Rust structs for composite / record types
// ---------------------------------------------------------------------------

/// A single synthesized Rust struct standing in for a PG composite type or an
/// anonymous `ROW(...)` / subquery record.
struct RecordType {
    /// The PG type this struct represents — a [`Type::Composite`] or
    /// [`Type::AnonymousRecord`]. Used as the dedup key.
    pg_type: Type,
    /// Generated struct name, e.g. `__TypedpgRecord0`.
    ident: proc_macro2::Ident,
    /// Decomposed fields, in declaration order.
    fields: Vec<RecordField>,
}

/// Collects every distinct composite / record type a query references so the
/// codegen can emit exactly one Rust struct per type (with hand-written
/// `FromSql` / `ToSql`) and reference it by name from columns, parameters and
/// nested fields.
///
/// Composite types pointed at a concrete Rust type via `[types]` are *not*
/// registered — that override wins and no struct is synthesized.
#[derive(Default)]
struct RecordRegistry {
    types: Vec<RecordType>,
}

impl RecordRegistry {
    /// Walk every column, parameter and spread field of a query, registering
    /// each composite / record type (and, recursively, the types nested
    /// inside it).
    fn build(analyzed: &AnalyzedQuery, config: &ResolvedConfig) -> RecordRegistry {
        let mut reg = RecordRegistry::default();
        for col in &analyzed.columns {
            reg.register(&col.pg_type, config);
        }
        for param in &analyzed.params {
            reg.register(&param.pg_type, config);
        }
        for spread in &analyzed.spreads {
            for field in &spread.fields {
                reg.register(&field.pg_type, config);
            }
        }
        reg
    }

    /// Register `ty` if it is (or wraps) a composite / record type.
    fn register(&mut self, ty: &Type, config: &ResolvedConfig) {
        match ty {
            Type::Domain { base, .. } => self.register(base, config),
            Type::Array { element } => self.register(element, config),
            Type::Range { subtype, .. } => self.register(subtype, config),
            Type::Composite { fields, .. } => {
                // Always synthesize a struct, even when a `[types]` override
                // exists — the synthesized struct is the `FromSql` / `ToSql`
                // decoder, and the override target is rebuilt from it.
                self.intern(ty, fields, config);
            }
            Type::AnonymousRecord { fields } => self.intern(ty, fields, config),
            Type::Basic { .. } | Type::Enum { .. } => {}
        }
    }

    /// Assign a struct name to `ty` (if not already known) and recurse into
    /// its fields. PG forbids a composite type from containing itself, so the
    /// recursion always terminates.
    fn intern(&mut self, ty: &Type, fields: &[RecordField], config: &ResolvedConfig) {
        if self.types.iter().any(|rt| &rt.pg_type == ty) {
            return;
        }
        let ident = format_ident!("__TypedpgRecord{}", self.types.len());
        self.types.push(RecordType {
            pg_type: ty.clone(),
            ident,
            fields: fields.to_vec(),
        });
        for field in fields {
            self.register(&field.ty, config);
        }
    }

    /// The synthesized struct name for `ty`, if one was registered.
    fn lookup(&self, ty: &Type) -> Option<&proc_macro2::Ident> {
        self.types
            .iter()
            .find(|rt| &rt.pg_type == ty)
            .map(|rt| &rt.ident)
    }
}

// ---------------------------------------------------------------------------
// Type mapping: PG Type -> Rust
// ---------------------------------------------------------------------------

/// Resolved Rust mapping for a PG [`Type`], used by both columns and params.
///
/// `strategy` tells the codegen how to (de)serialize the value at the
/// tokio-postgres boundary — plain `ToSql`/`FromSql`, JSONB-backed domain,
/// enum-as-string, or a collection thereof.
#[derive(Debug, Clone)]
struct RustMapping {
    rust_type: syn::Type,
    strategy: DeserStrategy,
    /// True when `rust_type` is a `Vec<T>` with a plain element type. In
    /// that case, param bindings can use `into_flex_vec` to accept any
    /// `IntoIterator<Item: Into<T>>` (e.g. `[&str; N]` for `Vec<String>`).
    accepts_iter: bool,
}

#[derive(Debug, Clone)]
enum DeserStrategy {
    /// Value implements `tokio_postgres::{ToSql, FromSql}` directly.
    /// `accepts_into_string` is set for text-like types so params can take
    /// `impl Into<String>`.
    Plain { accepts_into_string: bool },
    /// JSONB-backed domain. Value is serialized via `serde_json::to_value` on
    /// the way in and deserialized with `serde_json::from_value::<T>` on the
    /// way out.
    JsonbDomain { target: syn::Type },
    /// Enum represented as its label string. Value is stringified via
    /// `ToString` on the way in and parsed via `FromStr` on the way out.
    EnumAsString { target: syn::Type },
    /// Homogeneous collection of JSONB-backed domain values.
    VecOfJsonbDomain { inner: syn::Type },
    /// Homogeneous collection of enum values.
    VecOfEnumAsString { inner: syn::Type },
    /// Composite type or anonymous `ROW(...)` / subquery record. Decoded
    /// through a synthesized record struct (see [`RecordRegistry`]); when the
    /// composite has a `[types]` override the decoded value is rebuilt
    /// field-by-field into the user's struct. Valid for output columns only —
    /// using a composite value as a query parameter is rejected.
    Record,
    /// Homogeneous array of composite / record values.
    VecOfRecord,
}

/// Unwrap every `Domain` layer of `ty`, returning the innermost non-domain
/// type. Used to decide the (de)serialization strategy: a domain is just a
/// labelled wrapper, so the contract is dictated by the type it ultimately
/// wraps (a domain-over-domain-over-jsonb behaves like `jsonb`).
fn innermost_type(ty: &Type) -> &Type {
    match ty {
        Type::Domain { base, .. } => innermost_type(base),
        other => other,
    }
}

/// Walk `ty` and its domain bases, returning the first `[types]` override
/// found. A mapping keyed on a domain wins over one keyed on its base; if no
/// level is mapped the result is `None` and the default mapping applies.
fn override_path(ty: &Type, config: &ResolvedConfig) -> Option<String> {
    let lookup = |schema: &str, name: &str| {
        config
            .types
            .get(&QualifiedName::new(schema.to_string(), name.to_string()))
            .cloned()
    };
    match ty {
        Type::Domain {
            schema, name, base, ..
        } => lookup(schema, name).or_else(|| override_path(base, config)),
        Type::Composite { schema, name, .. }
        | Type::Enum { schema, name, .. }
        | Type::Basic { schema, name, .. }
        | Type::Range { schema, name, .. } => lookup(schema, name),
        Type::Array { .. } | Type::AnonymousRecord { .. } => None,
    }
}

/// True for `void` (behind any domains).
fn is_void(ty: &Type) -> bool {
    matches!(innermost_type(ty), Type::Basic { schema, name, .. }
        if schema == "pg_catalog" && name == "void")
}

/// True for the `json` / `jsonb` catalog types.
fn is_jsonb_type(ty: &Type) -> bool {
    matches!(ty, Type::Basic { name, .. } if name == "json" || name == "jsonb")
}

/// Entry point: resolve the Rust mapping for a PG [`Type`] at a given site.
///
/// The Rust type comes from the [`override_path`] walk (or a built-in default);
/// the (de)serialization [`DeserStrategy`] is dictated by the *innermost* base
/// type's kind — enum, JSONB, composite/record, or a plain scalar.
fn resolve_type_mapping(
    ty: &Type,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
) -> Result<RustMapping, syn::Error> {
    if let Type::Array { element } = ty {
        return resolve_array_mapping(element, config, registry);
    }

    let ovr = override_path(ty, config);

    match innermost_type(ty) {
        // A domain over an array behaves like the array itself.
        Type::Array { element } => resolve_array_mapping(element, config, registry),
        Type::Composite { .. } | Type::AnonymousRecord { .. } => Ok(RustMapping {
            rust_type: record_site_type(ty, config, registry)?,
            strategy: DeserStrategy::Record,
            accepts_iter: false,
        }),
        Type::Enum { .. } => {
            let target: syn::Type = match &ovr {
                Some(path) => parse_str(path)?,
                None => parse_str("String")?,
            };
            Ok(RustMapping {
                rust_type: target.clone(),
                strategy: DeserStrategy::EnumAsString { target },
                accepts_iter: false,
            })
        }
        inner @ Type::Basic {
            schema,
            name,
            extension,
            ..
        } => {
            if let Some(path) = &ovr {
                let target: syn::Type = parse_str(path)?;
                // A mapped JSONB type bridges through `serde`; any other
                // mapped scalar must implement `ToSql`/`FromSql` directly.
                let strategy = if is_jsonb_type(inner) {
                    DeserStrategy::JsonbDomain {
                        target: target.clone(),
                    }
                } else {
                    DeserStrategy::Plain {
                        accepts_into_string: false,
                    }
                };
                return Ok(RustMapping {
                    rust_type: target,
                    strategy,
                    accepts_iter: false,
                });
            }
            // No override: a known extension type, then a built-in.
            if let Some(ext) = extension.as_deref()
                && let Some(path) = pg_type_map::lookup_extension(ext, name)
            {
                return Ok(RustMapping {
                    rust_type: parse_str(path)?,
                    strategy: DeserStrategy::Plain {
                        accepts_into_string: false,
                    },
                    accepts_iter: false,
                });
            }
            if let Some(path) = pg_type_map::lookup_builtin(schema, name) {
                return Ok(RustMapping {
                    rust_type: parse_str(path)?,
                    strategy: DeserStrategy::Plain {
                        accepts_into_string: pg_type_map::is_string_like(schema, name),
                    },
                    accepts_iter: false,
                });
            }
            let qn = QualifiedName::new(schema.clone(), name.clone());
            Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "no Rust mapping for PostgreSQL type {qn} — cast it in the query to a \
                     type that has one (e.g. `::text`), or add it to \
                     [package.metadata.typedpg.types] in your Cargo.toml"
                ),
            ))
        }
        Type::Range {
            schema,
            name,
            subtype,
            multirange,
            ..
        } => {
            if let Some(path) = &ovr {
                return Ok(RustMapping {
                    rust_type: parse_str(path)?,
                    strategy: DeserStrategy::Plain {
                        accepts_into_string: false,
                    },
                    accepts_iter: false,
                });
            }
            let qn = QualifiedName::new(schema.clone(), name.clone());
            // tokio-postgres knows a user-defined multirange only as a
            // simple type, without its subtype: nothing can read it.
            if *multirange && schema != "pg_catalog" {
                return Err(syn::Error::new(
                    proc_macro2::Span::call_site(),
                    format!(
                        "no Rust mapping for PostgreSQL multirange type {qn} — add it to \
                         [package.metadata.typedpg.types] in your Cargo.toml"
                    ),
                ));
            }
            // No override: typedpg's Range<T> / MultiRange<T>, whose impls
            // read the subtype with T's.
            let inner = resolve_type_mapping(subtype, config, registry)?;
            if !matches!(inner.strategy, DeserStrategy::Plain { .. }) {
                return Err(syn::Error::new(
                    proc_macro2::Span::call_site(),
                    format!(
                        "no Rust mapping for PostgreSQL range type {qn}: its subtype is not a \
                         plain scalar — add it to [package.metadata.typedpg.types] in your \
                         Cargo.toml"
                    ),
                ));
            }
            let inner_rt = inner.rust_type;
            let wrapper = if *multirange { "MultiRange" } else { "Range" };
            Ok(RustMapping {
                rust_type: parse_str(&format!(
                    "::typedpg::types::{wrapper}<{}>",
                    quote::quote! { #inner_rt }
                ))?,
                strategy: DeserStrategy::Plain {
                    accepts_into_string: false,
                },
                accepts_iter: false,
            })
        }
        Type::Domain { .. } => unreachable!("innermost_type never returns a Domain"),
    }
}

/// Resolve the mapping for a PG array, given its element type.
fn resolve_array_mapping(
    element: &Type,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
) -> Result<RustMapping, syn::Error> {
    let inner = resolve_type_mapping(element, config, registry)?;
    let rt = &inner.rust_type;
    let vec_type: syn::Type = parse_str(&format!("Vec<{}>", quote! { #rt }))?;
    match inner.strategy {
        DeserStrategy::Plain { .. } => Ok(RustMapping {
            rust_type: vec_type,
            strategy: DeserStrategy::Plain {
                accepts_into_string: false,
            },
            accepts_iter: true,
        }),
        DeserStrategy::JsonbDomain { target } => Ok(RustMapping {
            rust_type: vec_type,
            strategy: DeserStrategy::VecOfJsonbDomain { inner: target },
            accepts_iter: false,
        }),
        DeserStrategy::EnumAsString { target } => Ok(RustMapping {
            rust_type: vec_type,
            strategy: DeserStrategy::VecOfEnumAsString { inner: target },
            accepts_iter: false,
        }),
        DeserStrategy::Record => Ok(RustMapping {
            rust_type: vec_type,
            strategy: DeserStrategy::VecOfRecord,
            accepts_iter: false,
        }),
        DeserStrategy::VecOfJsonbDomain { .. }
        | DeserStrategy::VecOfEnumAsString { .. }
        | DeserStrategy::VecOfRecord => Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "nested arrays of domain/enum/composite types are not supported",
        )),
    }
}

// ---------------------------------------------------------------------------
// Composite / record type resolution
// ---------------------------------------------------------------------------

/// The synthesized struct name reserved for a composite / record `ty`, as a
/// [`syn::Type`].
fn record_struct_path(ty: &Type, registry: &RecordRegistry) -> Result<syn::Type, syn::Error> {
    let ident = registry.lookup(ty).ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "internal error: composite/record type was not registered before codegen",
        )
    })?;
    Ok(syn::Type::Path(syn::TypePath {
        attrs: Vec::new(),
        qself: None,
        path: ident.clone().into(),
    }))
}

/// The Rust type a composite / record value decodes *into* on the wire — the
/// `FromSql`-capable representation. Composites always resolve to their
/// synthesized struct here, **ignoring** any `[types]` override, since the
/// override target is not assumed to implement `FromSql`.
fn record_raw_type(ty: &Type, registry: &RecordRegistry) -> Result<syn::Type, syn::Error> {
    match ty {
        Type::Composite { .. } | Type::AnonymousRecord { .. } => record_struct_path(ty, registry),
        Type::Domain { base, .. } => record_raw_type(base, registry),
        Type::Array { element } => {
            let inner = record_raw_type(element, registry)?;
            Ok(parse_str(&format!("Vec<{}>", quote! { #inner }))?)
        }
        // Reached only for a `Record`-strategy type, which always bottoms out
        // at a composite / anonymous record — never a scalar.
        _ => Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "internal error: record_raw_type on a non-record type",
        )),
    }
}

/// The Rust type a composite / record value is surfaced *as* — the `[types]`
/// override target where the [`override_path`] walk finds one, otherwise the
/// synthesized struct.
fn record_site_type(
    ty: &Type,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
) -> Result<syn::Type, syn::Error> {
    if let Some(path) = override_path(ty, config) {
        return parse_str(&path);
    }
    match innermost_type(ty) {
        inner @ (Type::Composite { .. } | Type::AnonymousRecord { .. }) => {
            record_struct_path(inner, registry)
        }
        Type::Array { element } => {
            let inner = record_site_type(element, config, registry)?;
            Ok(parse_str(&format!("Vec<{}>", quote! { #inner }))?)
        }
        _ => record_raw_type(ty, registry),
    }
}

/// Whether a decoded record value of `ty` must be rebuilt into a user struct —
/// true exactly when the composite type itself carries a `[types]` override.
/// Overrides on *nested* fields need no work here: the synthesized struct
/// already holds every field in its site-typed form.
fn record_needs_conversion(ty: &Type, config: &ResolvedConfig) -> bool {
    match ty {
        Type::Array { element } => record_needs_conversion(element, config),
        _ => override_path(ty, config).is_some(),
    }
}

/// Build an expression that turns a decoded value (`value`, of the synthesized
/// struct type) into its site type.
///
/// For a composite carrying a `[types]` override this rebuilds the user's
/// struct field-by-field — a flat move per field, since the synthesized
/// struct already holds each field in its final site-typed form. Without an
/// override it is the identity.
fn convert_record_to_site(
    value: TokenStream,
    ty: &Type,
    nullable: bool,
    config: &ResolvedConfig,
) -> Result<TokenStream, syn::Error> {
    if !record_needs_conversion(ty, config) {
        return Ok(value);
    }
    if let Type::Array { element } = ty {
        let elem = convert_record_to_site(quote! { __elem }, element, false, config)?;
        let map_vec = quote! {
            __vec.into_iter().map(|__elem| #elem).collect::<::std::vec::Vec<_>>()
        };
        return Ok(if nullable {
            quote! { #value.map(|__vec| #map_vec) }
        } else {
            quote! { { let __vec = #value; #map_vec } }
        });
    }

    // Composite (possibly behind domains) with an override: rebuild the
    // user's named struct from the synthesized record.
    let Some(path) = override_path(ty, config) else {
        return Ok(value);
    };
    let target: syn::Type = parse_str(&path)?;
    let fields = match innermost_type(ty) {
        Type::Composite { fields, .. } | Type::AnonymousRecord { fields } => fields,
        _ => return Ok(value),
    };
    let mut field_inits = TokenStream::new();
    for field in fields {
        let fname = make_field_ident(&field.name);
        field_inits.extend(quote! { #fname: __rec.#fname, });
    }
    let ctor = quote! { #target { #field_inits } };
    Ok(if nullable {
        quote! {
            match #value {
                ::std::option::Option::Some(__rec) => ::std::option::Option::Some(#ctor),
                ::std::option::Option::None => ::std::option::Option::None,
            }
        }
    } else {
        quote! { { let __rec = #value; #ctor } }
    })
}

/// Reject a composite / record value used as a query parameter. A composite
/// param would need its catalog OID at bind time, which the macro does not
/// have — callers should spell the value out with a `ROW(...)` constructor.
fn reject_record_param(strategy: &DeserStrategy) -> Result<(), syn::Error> {
    if matches!(strategy, DeserStrategy::Record | DeserStrategy::VecOfRecord) {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "composite / record-typed query parameters are not supported — \
             pass the fields individually using a ROW(...) constructor in SQL",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Record struct synthesis
// ---------------------------------------------------------------------------

/// Emit one Rust struct (plus its `FromSql` impl) for every composite and
/// anonymous record the query references.
fn emit_records(
    registry: &RecordRegistry,
    config: &ResolvedConfig,
) -> Result<TokenStream, syn::Error> {
    let mut out = TokenStream::new();
    for record in &registry.types {
        out.extend(emit_one_record(record, config, registry)?);
    }
    Ok(out)
}

/// Emit the struct definition and the hand-written `FromSql` impl for a single
/// composite / record type.
///
/// Each field is held in its *site* form — a custom enum, a JSONB-backed
/// struct, a nested record struct, a scalar — exactly as a top-level column of
/// that type would surface. The `FromSql` impl drives the same per-strategy
/// decoding [`decode_value`] generates for columns, sourced from a
/// [`RecordReader`] instead of a `Row`.
fn emit_one_record(
    record: &RecordType,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
) -> Result<TokenStream, syn::Error> {
    let ident = &record.ident;
    let count_lit = proc_macro2::Literal::usize_unsuffixed(record.fields.len());

    let mut struct_fields = TokenStream::new();
    let mut from_sql_reads = TokenStream::new();
    let mut from_sql_inits = TokenStream::new();

    for (i, field) in record.fields.iter().enumerate() {
        let fname = make_field_ident(&field.name);
        let mapping = resolve_type_mapping(&field.ty, config, registry)?;
        let site = &mapping.rust_type;
        let field_ty: syn::Type = if field.nullable {
            parse_str(&format!("::std::option::Option<{}>", quote! { #site }))?
        } else {
            mapping.rust_type.clone()
        };
        struct_fields.extend(quote! { pub #fname: #field_ty, });

        let decode = decode_value(
            &mapping,
            &field.ty,
            field.nullable,
            config,
            registry,
            &|raw, hint| match hint {
                Some(ty) => quote! { __reader.read_field_with::<#raw>(#ty)? },
                None => quote! { __reader.read_field::<#raw>()? },
            },
            DecodeCtx::RecordField(&format!("field \"{}\"", field.name)),
        )?;
        let tmp = format_ident!("__field_{}", i);
        from_sql_reads.extend(quote! { let #tmp: #field_ty = #decode; });
        from_sql_inits.extend(quote! { #fname: #tmp, });
    }

    let pg = quote! { ::typedpg::__private::tokio_postgres::types };
    let box_err = quote! {
        ::std::boxed::Box<dyn ::std::error::Error + ::std::marker::Send + ::std::marker::Sync>
    };

    Ok(quote! {
        #[derive(Debug, Clone)]
        #[allow(non_camel_case_types, dead_code)]
        pub struct #ident {
            #struct_fields
        }

        impl<'__typedpg_a> #pg::FromSql<'__typedpg_a> for #ident {
            fn from_sql(
                _ty: &#pg::Type,
                __raw: &'__typedpg_a [u8],
            ) -> ::std::result::Result<Self, #box_err> {
                let mut __reader = ::typedpg::__private::RecordReader::new(__raw)?;
                #from_sql_reads
                __reader.finish(#count_lit)?;
                ::std::result::Result::Ok(#ident { #from_sql_inits })
            }

            fn accepts(__ty: &#pg::Type) -> bool {
                ::typedpg::__private::record_accepts(__ty)
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub fn generate(
    analyzed: &AnalyzedQuery,
    config: &ResolvedConfig,
    executor_expr: &syn::Expr,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    if analyzed.spreads.is_empty() {
        generate_regular(analyzed, config, executor_expr, assignments)
    } else {
        generate_spread(analyzed, config, executor_expr, assignments)
    }
}

/// Generate the future a `copy_in!` invocation evaluates to: each item of
/// `source` becomes a row of `target`'s columns, read from the item's
/// `fields` in order, streamed through a binary `COPY ... FROM STDIN`.
pub fn generate_copy_in(
    target: &AnalyzedCopyIn,
    config: &ResolvedConfig,
    executor_expr: &syn::Expr,
    source: &syn::Expr,
    fields: &[syn::Ident],
) -> Result<TokenStream, syn::Error> {
    let registry = RecordRegistry::default();
    let mut pushes = TokenStream::new();
    for (column, field) in target.columns.iter().zip(fields) {
        pushes.extend(
            push_item_field(column, config, &registry, field).map_err(|e| {
                syn::Error::new(field.span(), format!("column \"{}\": {}", column.name, e))
            })?,
        );
    }
    let width = fields.len();
    let copy_sql = &target.copy_sql;
    let describe_sql = &target.describe_sql;
    Ok(quote! {{
        // Evaluated here, like sql!'s executor: the future takes the values,
        // not the variables they borrow from.
        let __executor = #executor_expr;
        let __source = #source;
        async move {
            // The rows are converted one at a time, as the COPY consumes
            // them: the source is never collected.
            let __rows = ::std::iter::IntoIterator::into_iter(__source).map(
                |__item| -> ::std::result::Result<typedpg::__private::CopyRow, typedpg::Error> {
                    let mut __params: typedpg::__private::CopyRow =
                        ::std::vec::Vec::with_capacity(#width);
                    #pushes
                    ::std::result::Result::Ok(__params)
                },
            );
            typedpg::Executor::copy_in(&__executor, #describe_sql, #copy_sql, __rows).await
        }
    }})
}

/// The `__TYPEDPG_QUERY` constant the `NoRows` / `TooManyRows` errors carry:
/// the query's SQL and the location of the `sql!` invocation (`file!()` and
/// friends, expanded with the call site's span, report where the macro was
/// invoked).
fn query_context(analyzed: &AnalyzedQuery) -> TokenStream {
    let sql = &analyzed.sql;
    quote! {
        #[allow(dead_code)]
        const __TYPEDPG_QUERY: typedpg::error::QueryContext = typedpg::error::QueryContext::new(
            #sql,
            ::core::file!(),
            ::core::line!(),
            ::core::column!(),
        );
    }
}

/// Generate code for a regular query (no spreads).
fn generate_regular(
    analyzed: &AnalyzedQuery,
    config: &ResolvedConfig,
    executor_expr: &syn::Expr,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    let registry = RecordRegistry::build(analyzed, config);
    let record_defs = emit_records(&registry, config)?;
    let output_struct = build_output_struct(&analyzed.columns, config, &registry)?;
    let (param_field_defs, param_field_inits) =
        build_param_fields(analyzed, config, &registry, assignments)?;
    let params_slice = build_params_slice(analyzed, config, &registry)?;
    let row_mapping = build_row_mapping(&analyzed.columns, config, &registry)?;

    let sql_str = cast_params(analyzed);
    let sql_limited =
        wrap_with_limit(&sql_str, analyzed.can_run_as_subquery).unwrap_or_else(|| sql_str.clone());

    let fetch_value_method = build_fetch_value_method(&analyzed.columns, config, &registry)?;
    let query_context = query_context(analyzed);

    let ts = quote! {
        {
            // ----- the query, for the errors about it -----
            #query_context

            // ----- synthesized composite / record structs -----
            #record_defs

            // ----- output type -----
            #[derive(Debug, Clone)]
            #[allow(non_camel_case_types)]
            struct __sql_output {
                #output_struct
            }

            // ----- query builder struct -----
            #[allow(non_camel_case_types)]
            struct __typedpg_query<__E: typedpg::Executor> {
                __executor: __E,
                #param_field_defs
            }

            // ----- method implementations -----
            impl<__E: typedpg::Executor> __typedpg_query<__E> {
                /// Execute the query and return all resulting rows.
                async fn fetch_all(self) -> ::std::result::Result<::std::vec::Vec<__sql_output>, typedpg::Error> {
                    let __rows = typedpg::Executor::query(
                        &self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await?;
                    __rows.into_iter().map(|__row| {
                        ::std::result::Result::Ok(__sql_output {
                            #row_mapping
                        })
                    }).collect()
                }

                /// Execute the query and return exactly one row.
                async fn fetch_one(self) -> ::std::result::Result<__sql_output, typedpg::Error> {
                    let __rows = typedpg::Executor::query(
                        &self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    let mut __iter = __rows.into_iter();
                    let __row = __iter.next()
                        .ok_or_else(|| typedpg::Error::no_rows(__TYPEDPG_QUERY))?;
                    if __iter.next().is_some() {
                        return ::std::result::Result::Err(typedpg::Error::too_many_rows(__TYPEDPG_QUERY));
                    }
                    ::std::result::Result::Ok(__sql_output {
                        #row_mapping
                    })
                }

                /// Execute the query and return at most one row.
                async fn fetch_optional(self) -> ::std::result::Result<::std::option::Option<__sql_output>, typedpg::Error> {
                    let __rows = typedpg::Executor::query(
                        &self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    let mut __iter = __rows.into_iter();
                    match __iter.next() {
                        Some(__row) => {
                            if __iter.next().is_some() {
                                return ::std::result::Result::Err(typedpg::Error::too_many_rows(__TYPEDPG_QUERY));
                            }
                            ::std::result::Result::Ok(Some(__sql_output {
                                #row_mapping
                            }))
                        },
                        None => ::std::result::Result::Ok(None),
                    }
                }

                /// Execute the query and stream its rows as the server sends
                /// them.
                async fn fetch_stream(self) -> ::std::result::Result<typedpg::QueryStream<__sql_output>, typedpg::Error> {
                    let __rows = typedpg::Executor::query_stream(
                        &self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await?;
                    ::std::result::Result::Ok(typedpg::QueryStream::new(__rows, |__row| {
                        ::std::result::Result::Ok(__sql_output {
                            #row_mapping
                        })
                    }))
                }

                /// Execute the query and stream its rows, mapped to `T`.
                async fn fetch_stream_as<__T: typedpg::FromRow>(self) -> ::std::result::Result<typedpg::QueryStream<__T>, typedpg::Error> {
                    let __rows = typedpg::Executor::query_stream(
                        &self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await?;
                    ::std::result::Result::Ok(typedpg::QueryStream::new(__rows, __T::from_row))
                }

                /// Execute the statement and return the number of affected rows.
                async fn execute(self) -> ::std::result::Result<u64, typedpg::Error> {
                    typedpg::Executor::execute(
                        &self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await
                }

                #fetch_value_method

                /// Execute the query and return all resulting rows mapped to `T`.
                async fn fetch_all_as<__T: typedpg::FromRow>(self) -> ::std::result::Result<::std::vec::Vec<__T>, typedpg::Error> {
                    let __rows = typedpg::Executor::query(
                        &self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await?;
                    __rows.into_iter().map(|__row| {
                        __T::from_row(&__row)
                    }).collect()
                }

                /// Execute the query and return exactly one row mapped to `T`.
                async fn fetch_one_as<__T: typedpg::FromRow>(self) -> ::std::result::Result<__T, typedpg::Error> {
                    let __rows = typedpg::Executor::query(
                        &self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    let mut __iter = __rows.into_iter();
                    let __row = __iter.next()
                        .ok_or_else(|| typedpg::Error::no_rows(__TYPEDPG_QUERY))?;
                    if __iter.next().is_some() {
                        return ::std::result::Result::Err(typedpg::Error::too_many_rows(__TYPEDPG_QUERY));
                    }
                    __T::from_row(&__row)
                }

                /// Execute the query and return at most one row mapped to `T`.
                async fn fetch_optional_as<__T: typedpg::FromRow>(self) -> ::std::result::Result<::std::option::Option<__T>, typedpg::Error> {
                    let __rows = typedpg::Executor::query(
                        &self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    let mut __iter = __rows.into_iter();
                    match __iter.next() {
                        Some(__row) => {
                            if __iter.next().is_some() {
                                return ::std::result::Result::Err(typedpg::Error::too_many_rows(__TYPEDPG_QUERY));
                            }
                            ::std::result::Result::Ok(Some(__T::from_row(&__row)?))
                        },
                        None => ::std::result::Result::Ok(None),
                    }
                }
            }

            // ----- construct and return the query builder -----
            __typedpg_query {
                __executor: #executor_expr,
                #param_field_inits
            }
        }
    };

    Ok(ts)
}

// ---------------------------------------------------------------------------
// Spread query code generation
// ---------------------------------------------------------------------------

fn generate_spread(
    analyzed: &AnalyzedQuery,
    config: &ResolvedConfig,
    executor_expr: &syn::Expr,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    let registry = RecordRegistry::build(analyzed, config);
    let record_defs = emit_records(&registry, config)?;
    let output_struct = build_output_struct(&analyzed.columns, config, &registry)?;
    let row_mapping = build_row_mapping(&analyzed.columns, config, &registry)?;
    let num_regular_params = analyzed.params.len();
    let num_spreads = analyzed.spreads.len();

    // ── Regular param fields ────────────────────────────────────────────
    let mut regular_param_fields = TokenStream::new();
    let mut regular_param_inits = TokenStream::new();
    let mut regular_param_pushes = TokenStream::new();
    for (idx, param) in analyzed.params.iter().enumerate() {
        let field_name = format_ident!("p{}", idx);
        let (field_type, value_expr) = build_field_type_and_value(
            param,
            config,
            &registry,
            &resolve_param_value(&param.name, assignments)?,
        )?;

        regular_param_fields.extend(quote! { #field_name: #field_type, });
        let param_ident = format_ident!("__{}", param.name);
        regular_param_inits.extend(quote! {
            #field_name: { let #param_ident: #field_type = #value_expr; #param_ident },
        });
        regular_param_pushes.extend(push_param(
            param,
            config,
            &registry,
            &quote! { self.#field_name },
        )?);
    }

    // ── Per-spread: generics, fields, inits, push exprs, SQL pieces ────
    let mut spread_struct_fields = TokenStream::new();
    let mut spread_struct_inits = TokenStream::new();
    let mut spread_empty_checks = TokenStream::new();
    let mut spread_param_pushes = TokenStream::new();
    let mut spread_size_args = TokenStream::new();
    let mut spread_size_params = TokenStream::new();

    // SQL pieces: the text between spread offsets
    let mut sql_pieces: Vec<&str> = Vec::new();
    let mut fields_per_row_lits = Vec::new();
    let mut last_offset = 0;

    for (si, spread) in analyzed.spreads.iter().enumerate() {
        let col_count = spread.fields.len();
        let field_ident = format_ident!("__spread_{}", si);
        let size_ident = format_ident!("__size_{}", si);

        // SQL piece before this spread
        sql_pieces.push(&analyzed.sql[last_offset..spread.offset]);
        last_offset = spread.offset;
        fields_per_row_lits.push(proc_macro2::Literal::usize_unsuffixed(col_count));

        // The spread's rows and parameters, extracted where `sql!` is
        // invoked — the one place the element type is concrete, so the
        // fields can be read (a method generic over the element type
        // can't access them).
        spread_struct_fields.extend(quote! {
            #field_ident: ::std::result::Result<
                (usize, ::std::vec::Vec<__SpreadParam>),
                typedpg::Error,
            >,
        });

        let spread_value_expr: TokenStream = {
            let assignment = assignments.iter().find(|a| a.name == spread.name);
            match assignment {
                Some(ParamAssignment { expr: Some(e), .. }) => quote! { #e },
                _ => {
                    let ident = format_ident!("{}", spread.name);
                    quote! { #ident }
                }
            }
        };
        // Param push expressions: iterate spread items and push field values
        let mut item_pushes = TokenStream::new();
        for field in &spread.fields {
            let accessor_ident = format_ident!("{}", field.name);
            item_pushes.extend(push_item_field(field, config, &registry, &accessor_ident)?);
        }

        spread_struct_inits.extend(quote! {
            #field_ident: (|| {
                let __items = &(#spread_value_expr)[..];
                let mut __params: ::std::vec::Vec<__SpreadParam> =
                    ::std::vec::Vec::with_capacity(__items.len() * #col_count);
                for __item in __items.iter() {
                    #item_pushes
                }
                ::std::result::Result::Ok((__items.len(), __params))
            })(),
        });

        let len_ident = format_ident!("__len_{}", si);
        let params_ident = format_ident!("__spread_params_{}", si);
        spread_empty_checks.extend(quote! {
            let (#len_ident, #params_ident) = self.#field_ident?;
            if #len_ident == 0 { __any_empty = true; }
        });

        spread_size_args.extend(quote! { #len_ident, });
        spread_size_params.extend(quote! { #size_ident: usize, });

        spread_param_pushes.extend(quote! {
            __params.extend(#params_ident);
        });
    }

    // Final SQL piece (after last spread)
    sql_pieces.push(&analyzed.sql[last_offset..]);

    // ── Generate the __build_spread_sql function body ────────────────────
    let num_regular_lit = proc_macro2::Literal::usize_unsuffixed(num_regular_params);
    let mut sql_builder_body = TokenStream::new();
    sql_builder_body.extend(quote! {
        let mut __sql = String::new();
        let mut __p: usize = #num_regular_lit + 1;
    });

    for si in 0..num_spreads {
        let piece = sql_pieces[si];
        let fpr = &fields_per_row_lits[si];
        let size_ident = format_ident!("__size_{}", si);
        sql_builder_body.extend(quote! {
            __sql.push_str(#piece);
            for __r in 0..#size_ident {
                if __r > 0 { __sql.push_str(", "); }
                __sql.push('(');
                for __c in 0..#fpr {
                    if __c > 0 { __sql.push_str(", "); }
                    __sql.push('$');
                    __sql.push_str(&__p.to_string());
                    __p += 1;
                }
                __sql.push(')');
            }
        });
    }
    let final_piece = sql_pieces[num_spreads];
    sql_builder_body.extend(quote! {
        __sql.push_str(#final_piece);
        __sql
    });

    // ── Capacity estimate ───────────────────────────────────────────────
    let mut capacity_expr = quote! { #num_regular_lit };
    for (si, fpr) in fields_per_row_lits.iter().enumerate() {
        let len_ident = format_ident!("__len_{}", si);
        capacity_expr.extend(quote! { + #len_ident * #fpr });
    }

    let query_preamble = quote! {
        let mut __any_empty = false;
        #spread_empty_checks
        let __sql = __build_spread_sql(#spread_size_args);
        let mut __params: Vec<__SpreadParam> = Vec::with_capacity(#capacity_expr);
        #regular_param_pushes
        #spread_param_pushes
        let __params_ref: Vec<&(dyn ::typedpg::__private::tokio_postgres::types::ToSql + Sync)>
            = __params
                .iter()
                .map(|p| p.as_ref() as &(dyn ::typedpg::__private::tokio_postgres::types::ToSql + Sync))
                .collect();
    };

    let fetch_value_method = build_fetch_value_method(&analyzed.columns, config, &registry)?;
    let query_context = query_context(analyzed);

    let ts = quote! {
        {
            // ----- the query, for the errors about it -----
            #query_context

            // ----- synthesized composite / record structs -----
            #record_defs

            #[derive(Debug, Clone)]
            #[allow(non_camel_case_types)]
            struct __sql_output {
                #output_struct
            }

            #[allow(non_camel_case_types)]
            // A parameter value, owned: `Send` so the query's future is.
            #[allow(non_camel_case_types)]
            type __SpreadParam =
                ::std::boxed::Box<dyn ::typedpg::__private::tokio_postgres::types::ToSql + Sync + Send>;

            #[allow(non_camel_case_types)]
            struct __typedpg_query<__E: typedpg::Executor> {
                __executor: __E,
                #spread_struct_fields
                #regular_param_fields
            }

            fn __build_spread_sql(#spread_size_params) -> String {
                #sql_builder_body
            }

            impl<__E: typedpg::Executor> __typedpg_query<__E> {
                async fn fetch_all(self) -> ::std::result::Result<::std::vec::Vec<__sql_output>, typedpg::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::vec::Vec::new());
                    }
                    let __rows = typedpg::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    __rows.into_iter().map(|__row| {
                        ::std::result::Result::Ok(__sql_output { #row_mapping })
                    }).collect()
                }

                async fn fetch_one(self) -> ::std::result::Result<__sql_output, typedpg::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Err(typedpg::Error::no_rows(__TYPEDPG_QUERY));
                    }
                    let __rows = typedpg::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    let mut __iter = __rows.into_iter();
                    let __row = __iter.next().ok_or_else(|| typedpg::Error::no_rows(__TYPEDPG_QUERY))?;
                    if __iter.next().is_some() {
                        return ::std::result::Result::Err(typedpg::Error::too_many_rows(__TYPEDPG_QUERY));
                    }
                    ::std::result::Result::Ok(__sql_output { #row_mapping })
                }

                async fn fetch_optional(self) -> ::std::result::Result<::std::option::Option<__sql_output>, typedpg::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::option::Option::None);
                    }
                    let __rows = typedpg::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    let mut __iter = __rows.into_iter();
                    match __iter.next() {
                        Some(__row) => {
                            if __iter.next().is_some() {
                                return ::std::result::Result::Err(typedpg::Error::too_many_rows(__TYPEDPG_QUERY));
                            }
                            ::std::result::Result::Ok(Some(__sql_output { #row_mapping }))
                        },
                        None => ::std::result::Result::Ok(None),
                    }
                }

                async fn fetch_stream(self) -> ::std::result::Result<typedpg::QueryStream<__sql_output>, typedpg::Error> {
                    #query_preamble
                    let __decode: fn(&typedpg::__private::tokio_postgres::Row) -> ::std::result::Result<__sql_output, typedpg::Error> =
                        |__row| ::std::result::Result::Ok(__sql_output { #row_mapping });
                    if __any_empty {
                        return ::std::result::Result::Ok(typedpg::QueryStream::new(typedpg::RowStream::empty(), __decode));
                    }
                    let __rows = typedpg::Executor::query_stream(&self.__executor, &__sql, &__params_ref).await?;
                    ::std::result::Result::Ok(typedpg::QueryStream::new(__rows, __decode))
                }

                async fn fetch_stream_as<__T: typedpg::FromRow>(self) -> ::std::result::Result<typedpg::QueryStream<__T>, typedpg::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(typedpg::QueryStream::new(typedpg::RowStream::empty(), __T::from_row));
                    }
                    let __rows = typedpg::Executor::query_stream(&self.__executor, &__sql, &__params_ref).await?;
                    ::std::result::Result::Ok(typedpg::QueryStream::new(__rows, __T::from_row))
                }

                async fn execute(self) -> ::std::result::Result<u64, typedpg::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(0);
                    }
                    typedpg::Executor::execute(&self.__executor, &__sql, &__params_ref).await
                }

                #fetch_value_method

                async fn fetch_all_as<__T: typedpg::FromRow>(self) -> ::std::result::Result<::std::vec::Vec<__T>, typedpg::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::vec::Vec::new());
                    }
                    let __rows = typedpg::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    __rows.into_iter().map(|__row| {
                        __T::from_row(&__row)
                    }).collect()
                }

                async fn fetch_one_as<__T: typedpg::FromRow>(self) -> ::std::result::Result<__T, typedpg::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Err(typedpg::Error::no_rows(__TYPEDPG_QUERY));
                    }
                    let __rows = typedpg::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    let mut __iter = __rows.into_iter();
                    let __row = __iter.next().ok_or_else(|| typedpg::Error::no_rows(__TYPEDPG_QUERY))?;
                    if __iter.next().is_some() {
                        return ::std::result::Result::Err(typedpg::Error::too_many_rows(__TYPEDPG_QUERY));
                    }
                    __T::from_row(&__row)
                }

                async fn fetch_optional_as<__T: typedpg::FromRow>(self) -> ::std::result::Result<::std::option::Option<__T>, typedpg::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::option::Option::None);
                    }
                    let __rows = typedpg::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    let mut __iter = __rows.into_iter();
                    match __iter.next() {
                        Some(__row) => {
                            if __iter.next().is_some() {
                                return ::std::result::Result::Err(typedpg::Error::too_many_rows(__TYPEDPG_QUERY));
                            }
                            ::std::result::Result::Ok(Some(__T::from_row(&__row)?))
                        },
                        None => ::std::result::Result::Ok(None),
                    }
                }
            }

            __typedpg_query {
                __executor: #executor_expr,
                #spread_struct_inits
                #regular_param_inits
            }
        }
    };

    Ok(ts)
}

// ---------------------------------------------------------------------------
// Helper: fetch_value() method (single-column queries only)
// ---------------------------------------------------------------------------

fn build_fetch_value_method(
    columns: &[AnalyzedColumn],
    config: &ResolvedConfig,
    registry: &RecordRegistry,
) -> Result<TokenStream, syn::Error> {
    if columns.len() != 1 {
        return Ok(TokenStream::new());
    }

    let col = &columns[0];
    let return_type = column_rust_type(col, config, registry)?;
    let field_name = make_field_ident(&col.name);

    let optional_body = if col.nullable {
        quote! {
            match self.fetch_optional().await? {
                Some(__v) => ::std::result::Result::Ok(__v.#field_name),
                None => ::std::result::Result::Ok(None),
            }
        }
    } else {
        quote! {
            match self.fetch_optional().await? {
                Some(__v) => ::std::result::Result::Ok(Some(__v.#field_name)),
                None => ::std::result::Result::Ok(None),
            }
        }
    };

    let optional_return_type = if col.nullable {
        quote! { #return_type }
    } else {
        quote! { ::std::option::Option<#return_type> }
    };

    Ok(quote! {
        async fn fetch_value(self) -> ::std::result::Result<#return_type, typedpg::Error> {
            let __v = self.fetch_one().await?;
            ::std::result::Result::Ok(__v.#field_name)
        }

        async fn fetch_value_optional(self) -> ::std::result::Result<#optional_return_type, typedpg::Error> {
            #optional_body
        }
    })
}

// ---------------------------------------------------------------------------
// Helper: output struct fields
// ---------------------------------------------------------------------------

fn build_output_struct(
    columns: &[AnalyzedColumn],
    config: &ResolvedConfig,
    registry: &RecordRegistry,
) -> Result<TokenStream, syn::Error> {
    let mut fields = TokenStream::new();

    for col in columns {
        let field_name = make_field_ident(&col.name);
        let field_type = column_rust_type(col, config, registry)?;

        fields.extend(quote! {
            pub #field_name: #field_type,
        });
    }

    Ok(fields)
}

// ---------------------------------------------------------------------------
// Helper: query struct param fields + initializer
// ---------------------------------------------------------------------------

fn build_param_fields(
    analyzed: &AnalyzedQuery,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
    assignments: &[ParamAssignment],
) -> Result<(TokenStream, TokenStream), syn::Error> {
    let mut defs = TokenStream::new();
    let mut inits = TokenStream::new();

    for (idx, param) in analyzed.params.iter().enumerate() {
        let field_name = format_ident!("p{}", idx);
        let value_expr = resolve_param_value(&param.name, assignments)?;
        let (field_type, value_expr) =
            build_field_type_and_value(param, config, registry, &value_expr)?;

        defs.extend(quote! {
            #field_name: #field_type,
        });

        let param_ident = format_ident!("__{}", param.name);
        inits.extend(quote! {
            #field_name: { let #param_ident: #field_type = #value_expr; #param_ident },
        });
    }

    Ok((defs, inits))
}

/// Push the value field `field` of the current row item (`__item`) supplies
/// for `param`, first bound to the Rust type `param` requires — with the
/// conversions a query parameter accepts — so a field of the wrong type is
/// a compile error at the field, not a failure when the query runs.
fn push_item_field<P: TypedParam>(
    param: &P,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
    field: &syn::Ident,
) -> Result<TokenStream, syn::Error> {
    let raw = quote::quote_spanned! {field.span()=> ::std::clone::Clone::clone(&__item.#field) };
    let (ty, value) = build_field_type_and_value(param, config, registry, &raw)?;
    let local = format_ident!("__field_{}", field);
    let push = push_param(param, config, registry, &quote! { #local })?;
    // The clone carries the field's span (so a type error points at the
    // field), which makes clippy treat it as the caller's code.
    Ok(quote::quote_spanned! {field.span()=>
        #[allow(clippy::clone_on_copy)]
        let #local: #ty = #value;
        #push
    })
}

/// Compute the Rust field type and (optionally wrapped) value expression for a
/// query parameter.
fn build_field_type_and_value<P: TypedParam>(
    param: &P,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
    value_expr: &TokenStream,
) -> Result<(syn::Type, TokenStream), syn::Error> {
    let mapping = resolve_type_mapping(param.pg_type(), config, registry)?;
    reject_record_param(&mapping.strategy)?;
    if is_void(param.pg_type()) {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "a query parameter can't be of type void",
        ));
    }
    let is_nullable = param.nullable();

    let inner_rt = &mapping.rust_type;
    let field_type: syn::Type = if is_nullable {
        parse_str(&format!("::std::option::Option<{}>", quote! { #inner_rt }))?
    } else {
        mapping.rust_type.clone()
    };

    let accepts_into_string = matches!(
        mapping.strategy,
        DeserStrategy::Plain {
            accepts_into_string: true
        }
    );
    let value_expr = match (accepts_into_string, mapping.accepts_iter, is_nullable) {
        (true, _, true) => {
            quote! {
                ::typedpg::__private::IntoOptionString::into_option_string(#value_expr)
            }
        }
        (true, _, false) => quote! { Into::<String>::into(#value_expr) },
        (_, true, false) => {
            // Vec<T> with a plain element — accept any IntoIterator<Item: Into<T>>.
            quote! { ::typedpg::__private::into_flex_vec(#value_expr) }
        }
        (false, _, true) => {
            quote! { ::std::option::Option::<#inner_rt>::from(#value_expr) }
        }
        (false, false, false) => quote! { Into::<#inner_rt>::into(#value_expr) },
    };

    Ok((field_type, value_expr))
}

/// Build the `push` statement for a param/field in the spread execution path.
///
/// `accessor` is the expression that evaluates to the value (e.g. `self.p0`
/// for regular params or `__item.name` for spread fields).
fn push_param<P: TypedParam>(
    param: &P,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
    accessor: &TokenStream,
) -> Result<TokenStream, syn::Error> {
    let mapping = resolve_type_mapping(param.pg_type(), config, registry)?;
    reject_record_param(&mapping.strategy)?;
    let is_nullable = param.nullable();
    let to_sql_ty = quote! {
        Box<dyn ::typedpg::__private::tokio_postgres::types::ToSql + Sync + Send>
    };

    let ts = match mapping.strategy {
        DeserStrategy::JsonbDomain { .. } => {
            if is_nullable {
                quote! {
                    __params.push(Box::new(match &#accessor {
                        Some(__v) => Some(::serde_json::to_value(__v)
                            .map_err(|e| typedpg::Error::Serialize(
                                format!("failed to serialize domain type to JSON: {e}")))?),
                        None => None,
                    }) as #to_sql_ty);
                }
            } else {
                quote! {
                    __params.push(Box::new(::serde_json::to_value(&#accessor)
                        .map_err(|e| typedpg::Error::Serialize(
                            format!("failed to serialize domain type to JSON: {e}")))?)
                        as #to_sql_ty);
                }
            }
        }
        DeserStrategy::EnumAsString { .. } => {
            if is_nullable {
                quote! {
                    __params.push(Box::new(#accessor.as_ref().map(|__v| __v.to_string()))
                        as #to_sql_ty);
                }
            } else {
                quote! {
                    __params.push(Box::new(#accessor.to_string()) as #to_sql_ty);
                }
            }
        }
        DeserStrategy::VecOfJsonbDomain { .. } => {
            if is_nullable {
                quote! {
                    __params.push(Box::new(match &#accessor {
                        Some(__vec) => Some(__vec.iter()
                            .map(|__v| ::serde_json::to_value(__v)
                                .map_err(|e| typedpg::Error::Serialize(
                                    format!("failed to serialize domain type to JSON: {e}"))))
                            .collect::<::std::result::Result<Vec<::serde_json::Value>, _>>()?),
                        None => None,
                    }) as #to_sql_ty);
                }
            } else {
                quote! {
                    __params.push(Box::new(#accessor.iter()
                        .map(|__v| ::serde_json::to_value(__v)
                            .map_err(|e| typedpg::Error::Serialize(
                                format!("failed to serialize domain type to JSON: {e}"))))
                        .collect::<::std::result::Result<Vec<::serde_json::Value>, _>>()?)
                        as #to_sql_ty);
                }
            }
        }
        DeserStrategy::VecOfEnumAsString { .. } => {
            if is_nullable {
                quote! {
                    __params.push(Box::new(#accessor.as_ref().map(|__vec|
                        __vec.iter().map(|__v| __v.to_string()).collect::<Vec<String>>()))
                        as #to_sql_ty);
                }
            } else {
                quote! {
                    __params.push(Box::new(
                        #accessor.iter().map(|__v| __v.to_string()).collect::<Vec<String>>())
                        as #to_sql_ty);
                }
            }
        }
        DeserStrategy::Plain { .. } => {
            quote! {
                __params.push(Box::new(::typedpg::__private::BaseTyped(#accessor.clone())) as #to_sql_ty);
            }
        }
        // `reject_record_param` above already bailed out for these.
        DeserStrategy::Record | DeserStrategy::VecOfRecord => unreachable!(),
    };
    Ok(ts)
}

/// Produces the value expression that will be stored in the query struct field.
fn resolve_param_value(
    param_name: &str,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    let assignment = assignments.iter().find(|a| a.name == param_name);

    match assignment {
        Some(ParamAssignment { expr: Some(e), .. }) => Ok(quote! { #e }),
        Some(ParamAssignment { expr: None, .. }) | None => {
            let ident = format_ident!("{}", param_name);
            Ok(quote! { #ident })
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: cast domain/enum params in SQL
// ---------------------------------------------------------------------------

fn cast_params(analyzed: &AnalyzedQuery) -> String {
    let mut insertions: Vec<(usize, String)> = Vec::new();

    for param in &analyzed.params {
        if let Some(pg_type) = param.pg_type.cast_name() {
            let cast_str = format!("::{pg_type}");
            for &offset in &param.sql_offsets {
                insertions.push((offset, cast_str.clone()));
            }
        }
    }

    if insertions.is_empty() {
        return analyzed.sql.clone();
    }

    insertions.sort_by_key(|(off, _)| *off);

    let sql = &analyzed.sql;
    let mut result = String::with_capacity(sql.len() + insertions.len() * 8);
    let mut last = 0;
    for (offset, cast_str) in &insertions {
        result.push_str(&sql[last..*offset]);
        result.push_str(cast_str);
        last = *offset;
    }
    result.push_str(&sql[last..]);

    result
}

// ---------------------------------------------------------------------------
// Helper: `&[&(dyn ToSql + Sync)]` slice
// ---------------------------------------------------------------------------

fn build_params_slice(
    analyzed: &AnalyzedQuery,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
) -> Result<TokenStream, syn::Error> {
    let mut elems = TokenStream::new();
    let to_sql = quote! {
        &(dyn ::typedpg::__private::tokio_postgres::types::ToSql + Sync)
    };

    for idx in 0..analyzed.params.len() {
        let field_name = format_ident!("p{}", idx);
        let pi = &analyzed.params[idx];
        let mapping = resolve_type_mapping(&pi.pg_type, config, registry)?;
        reject_record_param(&mapping.strategy)?;
        let nullable = pi.nullable;

        let elem = match mapping.strategy {
            DeserStrategy::JsonbDomain { .. } => {
                if nullable {
                    quote! {
                        &match &self.#field_name {
                            Some(__v) => Some(::serde_json::to_value(__v)
                                .map_err(|e| typedpg::Error::Serialize(
                                    format!("failed to serialize domain type to JSON: {e}")))?),
                            None => None,
                        } as #to_sql,
                    }
                } else {
                    quote! {
                        &::serde_json::to_value(&self.#field_name)
                            .map_err(|e| typedpg::Error::Serialize(
                                format!("failed to serialize domain type to JSON: {e}")))?
                            as #to_sql,
                    }
                }
            }
            DeserStrategy::EnumAsString { .. } => {
                if nullable {
                    quote! {
                        &self.#field_name.as_ref().map(|__v|
                            ::typedpg::__private::EnumString(__v.to_string()))
                            as #to_sql,
                    }
                } else {
                    quote! {
                        &::typedpg::__private::EnumString(self.#field_name.to_string())
                            as #to_sql,
                    }
                }
            }
            DeserStrategy::VecOfJsonbDomain { .. } => {
                if nullable {
                    quote! {
                        &match &self.#field_name {
                            Some(__vec) => Some(__vec.iter()
                                .map(|__v| ::serde_json::to_value(__v)
                                    .map_err(|e| typedpg::Error::Serialize(
                                        format!("failed to serialize domain type to JSON: {e}"))))
                                .collect::<::std::result::Result<Vec<::serde_json::Value>, _>>()?),
                            None => None,
                        } as #to_sql,
                    }
                } else {
                    quote! {
                        &self.#field_name.iter()
                            .map(|__v| ::serde_json::to_value(__v)
                                .map_err(|e| typedpg::Error::Serialize(
                                    format!("failed to serialize domain type to JSON: {e}"))))
                            .collect::<::std::result::Result<Vec<::serde_json::Value>, _>>()?
                            as #to_sql,
                    }
                }
            }
            DeserStrategy::VecOfEnumAsString { .. } => {
                if nullable {
                    quote! {
                        &self.#field_name.as_ref().map(|__vec|
                            __vec.iter().map(|__v| __v.to_string()).collect::<Vec<String>>())
                            as #to_sql,
                    }
                } else {
                    quote! {
                        &self.#field_name.iter()
                            .map(|__v| __v.to_string()).collect::<Vec<String>>()
                            as #to_sql,
                    }
                }
            }
            DeserStrategy::Plain { .. } => {
                quote! { &::typedpg::__private::BaseTyped(&self.#field_name) as #to_sql, }
            }
            // `reject_record_param` above already bailed out for these.
            DeserStrategy::Record | DeserStrategy::VecOfRecord => unreachable!(),
        };
        elems.extend(elem);
    }

    Ok(elems)
}

// ---------------------------------------------------------------------------
// Helper: row mapping inside `map(|__row| { ... })`
// ---------------------------------------------------------------------------

fn build_row_mapping(
    columns: &[AnalyzedColumn],
    config: &ResolvedConfig,
    registry: &RecordRegistry,
) -> Result<TokenStream, syn::Error> {
    let mut mappings = TokenStream::new();

    for (idx, col) in columns.iter().enumerate() {
        let field_name = make_field_ident(&col.name);
        let get_expr = column_get_expr(col, config, registry, idx)?;

        mappings.extend(quote! {
            #field_name: #get_expr,
        });
    }

    Ok(mappings)
}

// ---------------------------------------------------------------------------
// Type helpers
// ---------------------------------------------------------------------------

fn column_rust_type(
    col: &AnalyzedColumn,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
) -> Result<syn::Type, syn::Error> {
    let mapping = resolve_type_mapping(&col.pg_type, config, registry)?;
    let inner = mapping.rust_type;
    if col.nullable {
        Ok(parse_str(&format!(
            "::std::option::Option<{}>",
            quote! { #inner }
        ))?)
    } else {
        Ok(inner)
    }
}

/// Where a decoded value is being produced: determines the error type a
/// failed conversion must yield so that `?` is well-typed, and carries how
/// errors name the value.
#[derive(Clone, Copy)]
enum DecodeCtx<'a> {
    /// Inside the `sql!` row-mapping closure — errors are [`typedpg::Error`].
    /// Named like `column "x" (int4)`.
    Column(&'a str),
    /// Inside a synthesized record's `FromSql::from_sql` — errors are boxed.
    /// Named like `field "x"`.
    RecordField(&'a str),
}

impl DecodeCtx<'_> {
    /// The error, of this context's type, for a failure `e` to turn a value
    /// into `target`: `column "x" (jsonb): failed to deserialize T: …`.
    fn wrap_err(self, what: &str, target: &syn::Type) -> TokenStream {
        let (DecodeCtx::Column(label) | DecodeCtx::RecordField(label)) = self;
        let prefix = format!("{label}: {what} {}", type_text(target));
        let msg = quote! { format!("{}: {e}", #prefix) };
        match self {
            DecodeCtx::Column(_) => quote! { typedpg::Error::Deserialize(#msg) },
            DecodeCtx::RecordField(_) => quote! {
                <::std::boxed::Box<
                    dyn ::std::error::Error + ::std::marker::Send + ::std::marker::Sync,
                > as ::std::convert::From<::std::string::String>>::from(#msg)
            },
        }
    }
}

/// Build the expression that produces a site-typed value of `ty` from a raw
/// wire value, applying the [`DeserStrategy`]'s bridge.
///
/// `read(raw, hint)` turns a raw Rust type into an expression that reads a
/// value of that type from the underlying source — `read_column::<T>(…)?`
/// for an output column, `__reader.read_field*::<T>()?` for a synthesized
/// record field. `hint`, when set, is a PG `Type` expression the reader must
/// decode the value *as*: a record field carries only its inline OID, which
/// for a domain over `jsonb` does not resolve back to a built-in, so the
/// type-sensitive `serde_json::Value` decoder needs to be told. A column
/// reader gets the real type from the row description and ignores the hint.
fn decode_value(
    mapping: &RustMapping,
    ty: &Type,
    nullable: bool,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
    read: &dyn Fn(TokenStream, Option<TokenStream>) -> TokenStream,
    ctx: DecodeCtx<'_>,
) -> Result<TokenStream, syn::Error> {
    // PG `Type` a JSONB-strategy value must be decoded as — `json` vs `jsonb`
    // differ by a leading version byte.
    let json_hint = || {
        let variant = if matches!(innermost_type(ty), Type::Basic { name, .. } if name == "json") {
            quote! { JSON }
        } else {
            quote! { JSONB }
        };
        quote! { &::typedpg::__private::tokio_postgres::types::Type::#variant }
    };
    match &mapping.strategy {
        DeserStrategy::JsonbDomain { target } => {
            let err = ctx.wrap_err("failed to deserialize", target);
            let hint = Some(json_hint());
            if nullable {
                let rd = read(quote! { ::std::option::Option<::serde_json::Value> }, hint);
                Ok(quote! {
                    match #rd {
                        ::std::option::Option::Some(__v) => ::std::option::Option::Some(
                            ::serde_json::from_value::<#target>(__v).map_err(|e| #err)?),
                        ::std::option::Option::None => ::std::option::Option::None,
                    }
                })
            } else {
                let rd = read(quote! { ::serde_json::Value }, hint);
                Ok(quote! {
                    ::serde_json::from_value::<#target>(#rd).map_err(|e| #err)?
                })
            }
        }
        DeserStrategy::EnumAsString { target } => {
            let err = ctx.wrap_err("failed to parse enum", target);
            if nullable {
                let rd = read(
                    quote! { ::std::option::Option<::typedpg::__private::EnumString> },
                    None,
                );
                Ok(quote! {
                    match #rd {
                        ::std::option::Option::Some(__v) => ::std::option::Option::Some(
                            __v.0.parse::<#target>().map_err(|e| #err)?),
                        ::std::option::Option::None => ::std::option::Option::None,
                    }
                })
            } else {
                let rd = read(quote! { ::typedpg::__private::EnumString }, None);
                Ok(quote! {
                    { let __v = #rd; __v.0.parse::<#target>().map_err(|e| #err)? }
                })
            }
        }
        DeserStrategy::VecOfJsonbDomain { inner } => {
            let err = ctx.wrap_err("failed to deserialize", inner);
            let hint = Some(quote! {
                &::typedpg::__private::tokio_postgres::types::Type::JSONB_ARRAY
            });
            let map = quote! {
                __vs.into_iter()
                    .map(|__v| ::serde_json::from_value::<#inner>(__v).map_err(|e| #err))
                    .collect::<::std::result::Result<::std::vec::Vec<#inner>, _>>()?
            };
            if nullable {
                let rd = read(
                    quote! { ::std::option::Option<::std::vec::Vec<::serde_json::Value>> },
                    hint,
                );
                Ok(quote! {
                    match #rd {
                        ::std::option::Option::Some(__vs) => ::std::option::Option::Some(#map),
                        ::std::option::Option::None => ::std::option::Option::None,
                    }
                })
            } else {
                let rd = read(quote! { ::std::vec::Vec<::serde_json::Value> }, hint);
                Ok(quote! { { let __vs = #rd; #map } })
            }
        }
        DeserStrategy::VecOfEnumAsString { inner } => {
            let err = ctx.wrap_err("failed to parse enum", inner);
            let map = quote! {
                __vs.into_iter()
                    .map(|__v| __v.0.parse::<#inner>().map_err(|e| #err))
                    .collect::<::std::result::Result<::std::vec::Vec<#inner>, _>>()?
            };
            if nullable {
                let rd = read(
                    quote! { ::std::option::Option<::std::vec::Vec<::typedpg::__private::EnumString>> },
                    None,
                );
                Ok(quote! {
                    match #rd {
                        ::std::option::Option::Some(__vs) => ::std::option::Option::Some(#map),
                        ::std::option::Option::None => ::std::option::Option::None,
                    }
                })
            } else {
                let rd = read(
                    quote! { ::std::vec::Vec<::typedpg::__private::EnumString> },
                    None,
                );
                Ok(quote! { { let __vs = #rd; #map } })
            }
        }
        DeserStrategy::Record | DeserStrategy::VecOfRecord => {
            // Decode through the synthesized record struct, then (when the
            // composite type itself is mapped) rebuild the user's struct.
            let raw_type = record_raw_type(ty, registry)?;
            let rd = if nullable {
                read(quote! { ::std::option::Option<#raw_type> }, None)
            } else {
                read(quote! { #raw_type }, None)
            };
            let converted = convert_record_to_site(quote! { __raw }, ty, nullable, config)?;
            Ok(quote! {
                {
                    let __raw = #rd;
                    #converted
                }
            })
        }
        DeserStrategy::Plain { .. } if is_void(ty) => {
            // A `void` value has no bytes to decode; only NULL-ness is read.
            let rd = read(
                quote! { ::std::option::Option<::typedpg::__private::Void> },
                None,
            );
            Ok(if nullable {
                quote! { #rd.map(|_| ()) }
            } else {
                quote! { { let _ = #rd; } }
            })
        }
        DeserStrategy::Plain { .. } => {
            let base = &mapping.rust_type;
            if nullable {
                Ok(read(quote! { ::std::option::Option<#base> }, None))
            } else {
                Ok(read(quote! { #base }, None))
            }
        }
    }
}

fn column_get_expr(
    col: &AnalyzedColumn,
    config: &ResolvedConfig,
    registry: &RecordRegistry,
    idx: usize,
) -> Result<TokenStream, syn::Error> {
    let idx_lit = proc_macro2::Literal::usize_unsuffixed(idx);
    let mapping = resolve_type_mapping(&col.pg_type, config, registry)?;
    let name = &col.name;
    let pg_type = pg_type_label(&col.pg_type);
    let label = format!("column \"{name}\" ({pg_type})");
    decode_value(
        &mapping,
        &col.pg_type,
        col.nullable,
        config,
        registry,
        &|raw, _hint| {
            quote! { ::typedpg::__private::read_column::<#raw>(&__row, #idx_lit, #name, #pg_type)? }
        },
        DecodeCtx::Column(&label),
    )
}

/// A Rust type as source text, for messages: `::crate::T`, `Vec<i32>`.
fn type_text(ty: &syn::Type) -> String {
    quote!(#ty).to_string().replace(' ', "")
}

/// How an error message names a PG type: its name, schema-qualified (and
/// quoted where needed) outside `pg_catalog`, `[]`-suffixed for an array.
fn pg_type_label(ty: &Type) -> String {
    match ty {
        Type::Basic { schema, name, .. }
        | Type::Domain { schema, name, .. }
        | Type::Enum { schema, name, .. }
        | Type::Range { schema, name, .. }
        | Type::Composite { schema, name, .. } => {
            if schema == "pg_catalog" {
                name.clone()
            } else {
                QualifiedName::new(schema.clone(), name.clone()).to_string()
            }
        }
        Type::Array { element } => format!("{}[]", pg_type_label(element)),
        Type::AnonymousRecord { .. } => "record".to_string(),
    }
}
