//! The AST generated from PostgreSQL's headers must match the one the crate
//! commits.
//!
//! The committed `src/protobuf.rs`, `src/node.rs` and `src/equal.rs` were
//! produced by `cargo run -p typedpg_pg_query_codegen` from libpg_query's
//! `pg_query.proto`. [`generate_from_headers`] produces them from PostgreSQL
//! 18.7's headers instead, with no `.proto` and no `prost`. The walkers and
//! `equal()` implementations must come out identical (up to the file header),
//! and the AST types identical in structure — with the differences listed
//! below, each of which is checked, not assumed.
//!
//! The test needs PostgreSQL's configured headers, so it is `#[ignore]`d:
//!
//! ```text
//! TYPEDPG_PG_INCLUDE=/path/to/include \
//! TYPEDPG_PG_GRAM_H=/path/to/gram.h \
//! cargo test --release -p typedpg_pg_query_codegen --test from_headers -- --ignored
//! ```
//!
//! Run `cargo test -p typedpg_pg_query_codegen` for the naming unit tests in
//! `src/ast.rs`, which need no headers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use quote::ToTokens;
use syn::{Fields, ImplItem, Item, Type};

/// Fields libpg_query adds to the headers with its own patches
/// (`patches/15_node_string_constant_locations.patch`), so they are in the
/// committed AST but not in PostgreSQL's headers.
const PATCH_FIELDS: &[(&str, &str)] = &[
    ("DefElem", "arg_location"),
    ("NotifyStmt", "payload_location"),
    ("CreateSubscriptionStmt", "conninfo_location"),
    ("AlterSubscriptionStmt", "conninfo_location"),
];

/// Fields that are in the headers (both the vendored 18.6 ones and 18.7) but
/// not in libpg_query's committed `.proto` and `srcdata/struct_defs.json`:
/// those are stale relative to the headers they ship. A generator reading the
/// headers necessarily has the fields; there is no PostgreSQL 18.6→18.7
/// change to blame (`git diff` of the two headers shows the field in both).
const STALE_PROTO_FIELDS: &[(&str, &str)] = &[("CreateStatsStmt", "owner")];

/// The `Oid`-typed `owner` field is the one above; it is also why
/// `equal.rs` has `CreateStatsStmt`'s owner comparisons.
const STALE_PROTO_EQUAL_OWNER_IMPL: &str = "impl Equal for protobuf::CreateStatsStmt {";

/// `LimitOption` gained `LIMIT_OPTION_DEFAULT` in libpg_query
/// (`patches/05_limit_option_enum_value_default.patch`) but not in PostgreSQL:
/// the committed enum has `Default = 1, Count = 2, WithTies = 3`, the headers
/// `COUNT = 0, WITH_TIES = 1` (renumbered to `Count = 1, WithTies = 2`).
const LIMIT_OPTION_PATCH_VARIANT: &str = "Default";

/// `WCO_VIEW_CONFLICT_CHECK` was added to PostgreSQL's `WCOKind` after 18.6;
/// the headers have it, the committed AST (generated from the older proto)
/// doesn't. This is the only enum variant change in the scanned headers.
const POSTGRES_18_7_ENUM_VARIANT: (&str, &str) = ("WcoKind", "WcoViewConflictCheck");

#[test]
#[ignore = "needs PostgreSQL's generated headers; see the module docs"]
fn generated_sources_match_the_committed_ones() {
    let (Ok(include), Ok(gram_h)) = (
        std::env::var("TYPEDPG_PG_INCLUDE"),
        std::env::var("TYPEDPG_PG_GRAM_H"),
    ) else {
        eprintln!(
            "skipping: set TYPEDPG_PG_INCLUDE and TYPEDPG_PG_GRAM_H to PostgreSQL's \
             generated headers (see the module docs)"
        );
        return;
    };

    let crate_dir = typedpg_pg_query_codegen::crate_dir();
    let generated: BTreeMap<String, String> =
        typedpg_pg_query_codegen::generate_from_headers(Path::new(&include), Path::new(&gram_h))
            .into_iter()
            .map(|(path, content)| (path.display().to_string(), content))
            .collect();

    compare_text(
        "src/node.rs",
        generated.get("src/node.rs").expect("node.rs"),
        &read(&crate_dir, "src/node.rs"),
        |text| text.to_string(),
    );
    compare_text(
        "src/equal.rs",
        generated.get("src/equal.rs").expect("equal.rs"),
        &read(&crate_dir, "src/equal.rs"),
        normalize_create_stats_equal,
    );

    let committed = parse_api(&read(&crate_dir, "src/protobuf.rs"));
    let new = parse_api(generated.get("src/protobuf.rs").expect("protobuf.rs"));
    compare_api(&committed, &new);
}

/// Compare two generated files, ignoring the leading header comment (which
/// says what the file was generated from) and any normalisation of the body.
fn compare_text(name: &str, generated: &str, committed: &str, normalize: impl Fn(&str) -> String) {
    let generated = normalize(strip_header(generated));
    let committed = normalize(strip_header(committed));
    if generated != committed {
        let diff: Vec<String> = similar_lines(&committed, &generated);
        panic!(
            "{name} differs from the generated one:\n{}",
            diff.join("\n")
        );
    }
}

/// Drop the two `// @generated` / `// Do not edit;` lines and the blank line
/// after them.
fn strip_header(text: &str) -> &str {
    let mut lines = text.splitn(3, '\n');
    let first = lines.next().unwrap_or_default();
    assert!(
        first.starts_with("// @generated"),
        "no generated header: {first}"
    );
    let second = lines.next().unwrap_or_default();
    assert!(
        second.starts_with("// Do not edit;"),
        "no generated header: {second}"
    );
    lines
        .next()
        .unwrap_or_default()
        .strip_prefix('\n')
        .unwrap_or_default()
}

/// `CreateStatsStmt.owner` adds two lines to its `impl Equal` (see
/// [`STALE_PROTO_FIELDS`]); drop them from either side so the rest is
/// compared verbatim.
fn normalize_create_stats_equal(text: &str) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with(STALE_PROTO_EQUAL_OWNER_IMPL) {
            inside = true;
        }
        if inside && (line.contains("self.owner.equal(") || line.contains("self.owner.equal_key("))
        {
            continue;
        }
        out.push(line);
        if inside && line == "}" {
            inside = false;
        }
    }
    let mut text = out.join("\n");
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

fn read(crate_dir: &Path, relative: &str) -> String {
    let path = crate_dir.join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// A poor man's line diff, to keep the failure readable.
fn similar_lines(expected: &str, actual: &str) -> Vec<String> {
    let expected: Vec<&str> = expected.lines().collect();
    let actual: Vec<&str> = actual.lines().collect();
    let mut out = Vec::new();
    for (i, (a, b)) in expected.iter().zip(&actual).enumerate() {
        if a != b {
            out.push(format!(
                "line {}:\n  committed: {a}\n  generated: {b}",
                i + 1
            ));
            if out.len() == 10 {
                out.push("…".to_string());
                break;
            }
        }
    }
    if expected.len() != actual.len() {
        out.push(format!(
            "line counts differ: committed {}, generated {}",
            expected.len(),
            actual.len()
        ));
    }
    out
}

#[derive(Default, Debug)]
struct Api {
    structs: BTreeMap<String, Vec<FieldInfo>>,
    enums: BTreeMap<String, Vec<VariantInfo>>,
    /// Inherent methods by type, as normalised signatures.
    methods: BTreeMap<String, BTreeSet<String>>,
    /// Trait impls by type, as the trait's normalised path.
    traits: BTreeMap<String, BTreeSet<String>>,
    /// `#[derive(...)]` entries by type.
    derives: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldInfo {
    name: String,
    ty: String,
    /// The enum type of a `#[prost(enumeration = "…")]` field (prost stored
    /// enum fields as `i32` and derived accessors from this attribute).
    enumeration: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VariantInfo {
    name: String,
    discriminant: Option<String>,
}

fn parse_api(source: &str) -> Api {
    let file = syn::parse_file(source).unwrap_or_else(|e| panic!("parsing generated Rust: {e}"));
    let mut api = Api::default();
    walk(&file.items, "", &mut api);
    api
}

fn walk(items: &[Item], prefix: &str, api: &mut Api) {
    for item in items {
        match item {
            Item::Struct(item) => {
                let name = join_path(prefix, &item.ident.to_string());
                let fields = match &item.fields {
                    Fields::Named(fields) => fields
                        .named
                        .iter()
                        .map(|f| FieldInfo {
                            name: f.ident.as_ref().expect("a named field").to_string(),
                            ty: normalize_type(&f.ty),
                            enumeration: enumeration_of(&f.attrs),
                        })
                        .collect(),
                    Fields::Unnamed(fields) => fields
                        .unnamed
                        .iter()
                        .enumerate()
                        .map(|(i, f)| FieldInfo {
                            name: i.to_string(),
                            ty: normalize_type(&f.ty),
                            enumeration: enumeration_of(&f.attrs),
                        })
                        .collect(),
                    Fields::Unit => Vec::new(),
                };
                api.derives.insert(name.clone(), derive_names(&item.attrs));
                api.structs.insert(name, fields);
            }
            Item::Enum(item) => {
                let name = join_path(prefix, &item.ident.to_string());
                let variants = item
                    .variants
                    .iter()
                    .map(|v| VariantInfo {
                        name: v.ident.to_string(),
                        discriminant: v.discriminant.as_ref().map(|(_, expr)| {
                            expr.to_token_stream()
                                .to_string()
                                .chars()
                                .filter(|c| !c.is_whitespace())
                                .collect()
                        }),
                    })
                    .collect();
                api.derives.insert(name.clone(), derive_names(&item.attrs));
                api.enums.insert(name, variants);
            }
            Item::Mod(item) => {
                if let Some((_, items)) = &item.content {
                    walk(items, &join_path(prefix, &item.ident.to_string()), api);
                }
            }
            Item::Impl(item) => {
                let self_ty = join_path(prefix, &normalize_type(&item.self_ty));
                if let Some((_, trait_path, _)) = &item.trait_ {
                    api.traits.entry(self_ty).or_default().insert(
                        trait_path
                            .to_token_stream()
                            .to_string()
                            .chars()
                            .filter(|c| !c.is_whitespace())
                            .collect(),
                    );
                } else {
                    for member in &item.items {
                        if let ImplItem::Fn(function) = member {
                            api.methods
                                .entry(self_ty.clone())
                                .or_default()
                                .insert(signature(&function.sig));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// The entries of a `#[derive(...)]` attribute, whitespace-free.
fn derive_names(attrs: &[syn::Attribute]) -> BTreeSet<String> {
    for attr in attrs {
        let text = attr.to_token_stream().to_string();
        let Some(rest) = text.strip_prefix("# [derive (") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(")]") else {
            continue;
        };
        return rest
            .split(',')
            .map(|name| name.chars().filter(|c| !c.is_whitespace()).collect())
            .filter(|name: &String| !name.is_empty())
            .collect();
    }
    BTreeSet::new()
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

/// The `#[prost(enumeration = "Name", …)]` type name, if any.
fn enumeration_of(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        let text = attr.to_token_stream().to_string();
        if let Some(rest) = text.split("enumeration").nth(1)
            && let Some(start) = rest.find('"')
            && let Some(end) = rest[start + 1..].find('"')
        {
            return Some(rest[start + 1..start + 1 + end].to_string());
        }
    }
    None
}

/// A type as a comparable string: `prost`'s `alloc` paths and `std`'s are the
/// same type, and `Option`/`Vec`/`Box` are written fully qualified on one
/// side of the comparison and not always on the other.
fn normalize_type(ty: &Type) -> String {
    let mut text: String = ty
        .to_token_stream()
        .to_string()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    for (from, to) in [
        ("::prost::alloc::string::String", "String"),
        ("::std::string::String", "String"),
        ("::prost::alloc::vec::Vec", "Vec"),
        ("::std::vec::Vec", "Vec"),
        ("::prost::alloc::boxed::Box", "Box"),
        ("::std::boxed::Box", "Box"),
        ("::core::option::Option", "Option"),
        ("::std::collections::HashMap", "HashMap"),
        ("::prost::alloc::collections::BTreeMap", "BTreeMap"),
    ] {
        text = text.replace(from, to);
    }
    text
}

/// An inherent method's signature, whitespace-free.
fn signature(sig: &syn::Signature) -> String {
    sig.to_token_stream()
        .to_string()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

fn compare_api(committed: &Api, generated: &Api) {
    // The generator defines its own `TryFrom` error (prost's
    // `UnknownEnumValue` goes away with `prost`).
    assert!(
        generated.structs.contains_key("UnknownEnumValue"),
        "generated protobuf.rs has no UnknownEnumValue"
    );
    assert!(
        !committed.structs.contains_key("UnknownEnumValue"),
        "committed protobuf.rs unexpectedly has UnknownEnumValue"
    );

    let struct_names = |api: &Api| -> BTreeSet<String> {
        api.structs
            .keys()
            .filter(|name| *name != "UnknownEnumValue")
            .cloned()
            .collect()
    };
    assert_eq!(
        struct_names(committed),
        struct_names(generated),
        "the generated structs differ"
    );
    assert_eq!(
        committed.enums.keys().collect::<Vec<_>>(),
        generated.enums.keys().collect::<Vec<_>>(),
        "the generated enums differ"
    );

    for (name, committed_fields) in &committed.structs {
        // Fields libpg_query's patches add: present in the committed AST,
        // absent from the generated one.
        let mut expected: Vec<FieldInfo> = committed_fields.clone();
        expected.retain(|f| !PATCH_FIELDS.contains(&(name.as_str(), f.name.as_str())));

        // Fields the stale committed proto lacks: absent there, present here.
        let mut actual = generated.structs.get(name).expect("struct").clone();
        for (_, field) in STALE_PROTO_FIELDS.iter().filter(|(ty, _)| ty == name) {
            let position = actual
                .iter()
                .position(|f| f.name == *field)
                .unwrap_or_else(|| panic!("{name}.{field} missing from generated"));
            assert!(
                !committed_fields.iter().any(|f| f.name == *field),
                "{name}.{field} is no longer stale in the committed AST; update the test"
            );
            actual.remove(position);
        }
        let pairs = |fields: &[FieldInfo]| -> Vec<(String, String)> {
            fields
                .iter()
                .map(|f| (f.name.clone(), f.ty.clone()))
                .collect()
        };
        assert_eq!(
            pairs(&expected),
            pairs(&actual),
            "struct {name} fields differ"
        );
    }

    for (name, field) in PATCH_FIELDS {
        let committed = committed.structs.get(*name).expect("struct");
        let generated = generated.structs.get(*name).expect("struct");
        assert!(
            committed.iter().any(|f| f.name == *field),
            "{name}.{field} is not a libpg_query patch field any more"
        );
        assert!(
            !generated.iter().any(|f| f.name == *field),
            "{name}.{field} is no longer a libpg_query patch field"
        );
    }

    for (name, committed_variants) in &committed.enums {
        let generated_variants = generated.enums.get(name).expect("enum");
        if name == "Token" {
            compare_tokens(committed_variants, generated_variants);
            continue;
        }
        let mut expected = committed_variants.clone();
        if name == "LimitOption" {
            let position = expected
                .iter()
                .position(|v| v.name == LIMIT_OPTION_PATCH_VARIANT)
                .unwrap_or_else(|| {
                    panic!("LimitOption lost its libpg_query-patched {LIMIT_OPTION_PATCH_VARIANT}")
                });
            expected.remove(position);
            for variant in &mut expected[position..] {
                let value: i64 = variant
                    .discriminant
                    .as_ref()
                    .expect("LimitOption values have discriminants")
                    .parse()
                    .expect("a decimal discriminant");
                variant.discriminant = Some((value - 1).to_string());
            }
        }
        let mut actual = generated_variants.clone();
        let (enum_name, variant) = POSTGRES_18_7_ENUM_VARIANT;
        if name == enum_name {
            let position = actual
                .iter()
                .position(|v| v.name == variant)
                .unwrap_or_else(|| panic!("{enum_name} lost {variant}"));
            actual.remove(position);
            assert!(
                !committed_variants.iter().any(|v| v.name == variant),
                "{enum_name}::{variant} is no longer a PostgreSQL 18.7 addition"
            );
        }
        assert_eq!(expected, actual, "enum {name} variants differ");
    }

    // Derives: `prost`'s own are replaced by `Debug`/`Default` derives.
    for (name, current) in &committed.derives {
        if name == "UnknownEnumValue" {
            continue;
        }
        let mut expected = current.clone();
        let had_message = expected.remove("::prost::Message");
        let had_oneof = expected.remove("::prost::Oneof");
        let had_enumeration = expected.remove("::prost::Enumeration");
        if had_message || had_oneof || had_enumeration {
            expected.insert("Debug".to_string());
        }
        if had_message {
            expected.insert("Default".to_string());
        }
        let actual = generated.derives.get(name).cloned().unwrap_or_default();
        assert_eq!(expected, actual, "derives of {name} differ");
    }

    // Inherent methods: the committed file gets its accessors from
    // `#[derive(::prost::Message)]`, so synthesize what prost-derive emits and
    // compare with the ones the generator writes out.
    let types: BTreeSet<String> = committed
        .methods
        .keys()
        .chain(generated.methods.keys())
        .chain(committed.structs.keys())
        .chain(committed.enums.keys())
        .filter(|name| *name != "UnknownEnumValue")
        .cloned()
        .collect();
    for ty in types {
        let mut expected = committed.methods.get(&ty).cloned().unwrap_or_default();
        if let Some(fields) = committed.structs.get(&ty) {
            for field in fields {
                let Some(enum_ty) = &field.enumeration else {
                    continue;
                };
                let ident = &field.name;
                let set_ident = ident.strip_prefix("r#").unwrap_or(ident);
                expected.insert(format!("fn{ident}(&self)->{enum_ty}"));
                expected.insert(format!("fnset_{set_ident}(&mutself,value:{enum_ty})"));
            }
        }
        let actual = generated.methods.get(&ty).cloned().unwrap_or_default();
        assert_eq!(expected, actual, "inherent methods of {ty} differ");
    }

    // Every enum gets `TryFrom<i32>`, which `#[derive(::prost::Enumeration)]`
    // used to provide.
    for (name, variants) in &committed.enums {
        // Oneof enums carry no discriminants and get no `TryFrom`.
        if variants.iter().any(|v| v.discriminant.is_none()) {
            continue;
        }
        let traits = generated.traits.get(name).cloned().unwrap_or_default();
        assert!(
            traits.iter().any(|t| t.contains("TryFrom<i32>")),
            "generated enum {name} has no TryFrom<i32>"
        );
    }
}

/// `Token` comparison: the committed enum follows libpg_query's patched
/// grammar, the generated one `gram.h`'s.
///
/// `patches/04_lexer_comments_as_tokens.patch` inserts `SQL_COMMENT` and
/// `C_COMMENT` after `NOT_EQUALS` (274), so every later token in the committed
/// enum is numbered two higher than in `gram.h`. PostgreSQL's numbers are
/// kept, and the two comment tokens — whose old numbers 275/276 are taken by
/// `ABORT_P`/`ABSENT` in `gram.h` — are numbered after the highest token.
fn compare_tokens(committed: &[VariantInfo], generated: &[VariantInfo]) {
    assert_eq!(
        committed.iter().map(|v| &v.name).collect::<Vec<_>>(),
        generated.iter().map(|v| &v.name).collect::<Vec<_>>(),
        "the Token variants differ"
    );
    let value = |variant: &VariantInfo| -> i64 {
        variant
            .discriminant
            .as_ref()
            .unwrap_or_else(|| panic!("Token::{} has no discriminant", variant.name))
            .parse()
            .expect("a decimal Token discriminant")
    };
    for (old, new) in committed.iter().zip(generated) {
        let old_value = value(old);
        let new_value = value(new);
        match old.name.as_str() {
            // The scanner synthesizes these; they are numbered after the
            // highest `gram.h` token rather than libpg_query's 275/276.
            "SqlComment" | "CComment" => assert!(
                new_value > 779,
                "Token::{} should be numbered after the highest gram.h token, not {new_value}",
                new.name
            ),
            _ if old_value >= 277 => assert_eq!(
                old_value - 2,
                new_value,
                "Token::{} should keep gram.h's number (current {} - 2)",
                old.name,
                old_value
            ),
            _ => assert_eq!(
                old_value, new_value,
                "Token::{} should be unchanged",
                old.name
            ),
        }
    }
}
