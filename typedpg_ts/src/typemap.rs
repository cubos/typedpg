//! PostgreSQL → TypeScript type mapping.
//!
//! Every column is read in PG's text format, so each value reaches the
//! runtime as a string and a [`Codec`] turns it into the JS value its TS
//! type promises. A type with no specific mapping stays a `string`: its text
//! output is always one, so the fallback is sound (unlike the Rust side,
//! which reads the binary format and must know every type it decodes).
//!
//! The runtime's own types are written `typedpg.Range<…>` /
//! `typedpg.Interval` / `typedpg.JsonValue`: the generated module imports
//! them as a namespace, so a type the user maps to can't collide with them.

use std::collections::HashMap;

use typedpg_analyzer::{RecordField, Type};
use typedpg_core::QualifiedName;

use crate::config::Int8;

/// The namespace the generated module imports the runtime's types under.
pub const RUNTIME_NS: &str = "typedpg";

/// How the runtime turns a column's text value into a JS value, and a JS
/// parameter value into text. Serialized into the generated module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    /// The text as is.
    Text,
    /// `t` / `f` ↔ `boolean`.
    Bool,
    /// Integers and floats that fit a JS `number` (`NaN` and the
    /// infinities included).
    Number,
    /// `int8` / `xid8` ↔ `bigint`.
    BigInt,
    /// `int8` ↔ `number`, an error beyond 2^53 rather than a rounded value.
    Int8Number,
    /// `json` / `jsonb`, parsed / stringified.
    Json,
    /// `timestamptz` ↔ `Date`.
    Timestamptz,
    /// `bytea` ↔ `Uint8Array`.
    Bytea,
    /// `interval` ↔ `{ months, days, microseconds }`.
    Interval,
    /// pgvector's `vector` / `halfvec` (`[1,2,3]`) ↔ `number[]`.
    Vector,
    /// `hstore` (`"a"=>"1"`) ↔ `Record<string, string | null>`.
    Hstore,
    /// `void`: no value.
    Void,
    /// A PG array of the element codec.
    Array(Box<Codec>),
    /// A composite or anonymous record, field by field.
    Record(Vec<(String, Codec)>),
    /// A range over the subtype's codec.
    Range(Box<Codec>),
    /// A multirange: the ranges, in order.
    Multirange(Box<Codec>),
}

impl Codec {
    /// The codec as the runtime reads it: a name, or `[kind, …]` for the
    /// ones built of others.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{Value, json};
        match self {
            Codec::Text => json!("text"),
            Codec::Bool => json!("bool"),
            Codec::Number => json!("number"),
            Codec::BigInt => json!("bigint"),
            Codec::Int8Number => json!("int8number"),
            Codec::Json => json!("json"),
            Codec::Timestamptz => json!("timestamptz"),
            Codec::Bytea => json!("bytea"),
            Codec::Interval => json!("interval"),
            Codec::Vector => json!("vector"),
            Codec::Hstore => json!("hstore"),
            Codec::Void => json!("void"),
            Codec::Array(inner) => json!(["array", inner.to_json()]),
            Codec::Range(inner) => json!(["range", inner.to_json()]),
            Codec::Multirange(inner) => json!(["multirange", inner.to_json()]),
            Codec::Record(fields) => json!([
                "record",
                fields
                    .iter()
                    .map(|(name, codec)| json!([name, codec.to_json()]))
                    .collect::<Vec<Value>>()
            ]),
        }
    }
}

/// A PG type's TypeScript mapping.
#[derive(Debug, Clone)]
pub struct TsType {
    /// The type of the value a column of it decodes to.
    pub output: String,
    /// The type a parameter of it accepts — looser where several JS values
    /// encode the same way (`number` for an `int8`, `string` for a
    /// `timestamptz`).
    pub input: String,
    pub codec: Codec,
}

/// Maps analyzer types to TypeScript, honoring the configured overrides.
pub struct TypeMapper<'a> {
    /// PG type → the TS type name the generated module imports for it.
    pub overrides: &'a HashMap<QualifiedName, String>,
    pub int8: Int8,
}

impl TypeMapper<'_> {
    /// The mapping of a value of `ty` (its own nullability is the caller's).
    pub fn map(&self, ty: &Type) -> TsType {
        match ty {
            Type::Basic {
                schema,
                name,
                extension,
                ..
            } => {
                let mapped = self.basic(schema, name, extension.as_deref());
                match self.override_for(schema, name) {
                    Some(ts) => same(ts, mapped.codec),
                    None => mapped,
                }
            }
            Type::Domain {
                schema, name, base, ..
            } => {
                let base = self.map(base);
                match self.override_for(schema, name) {
                    Some(ts) => same(ts, base.codec),
                    None => base,
                }
            }
            Type::Enum {
                schema,
                name,
                labels,
                ..
            } => {
                let ts = self.override_for(schema, name).unwrap_or_else(|| {
                    if labels.is_empty() {
                        "never".to_owned()
                    } else {
                        labels
                            .iter()
                            .map(|l| js_string(l))
                            .collect::<Vec<_>>()
                            .join(" | ")
                    }
                });
                same(ts, Codec::Text)
            }
            Type::Range {
                schema,
                name,
                subtype,
                multirange,
                ..
            } => {
                let sub = self.map(subtype);
                let (output, input, codec) = if *multirange {
                    (
                        format!("{RUNTIME_NS}.Range<{}>[]", sub.output),
                        format!("readonly {RUNTIME_NS}.Range<{}>[]", sub.input),
                        Codec::Multirange(Box::new(sub.codec)),
                    )
                } else {
                    (
                        format!("{RUNTIME_NS}.Range<{}>", sub.output),
                        format!("{RUNTIME_NS}.Range<{}>", sub.input),
                        Codec::Range(Box::new(sub.codec)),
                    )
                };
                match self.override_for(schema, name) {
                    Some(ts) => same(ts, codec),
                    None => TsType {
                        output,
                        input,
                        codec,
                    },
                }
            }
            Type::Array {
                element,
                element_nullable,
            } => {
                let elem = self.map(element);
                // An element is nullable unless the analysis proved it isn't.
                let out_elem = if *element_nullable == Some(false) {
                    elem.output.clone()
                } else {
                    format!("{} | null", elem.output)
                };
                TsType {
                    output: array_of(&out_elem),
                    input: format!("readonly {}", array_of(&format!("{} | null", elem.input))),
                    codec: Codec::Array(Box::new(elem.codec)),
                }
            }
            Type::Composite {
                schema,
                name,
                fields,
                ..
            } => {
                let record = self.record(fields);
                match self.override_for(schema, name) {
                    Some(ts) => same(ts, record.codec),
                    None => record,
                }
            }
            Type::AnonymousRecord { fields } => self.record(fields),
        }
    }

    fn record(&self, fields: &[RecordField]) -> TsType {
        let mut output = Vec::with_capacity(fields.len());
        let mut input = Vec::with_capacity(fields.len());
        let mut codecs = Vec::with_capacity(fields.len());
        for field in fields {
            let mapped = self.map(&field.ty);
            let null = if field.nullable { " | null" } else { "" };
            let key = property_key(&field.name);
            output.push(format!("{key}: {}{null}", mapped.output));
            input.push(format!("{key}: {}{null}", mapped.input));
            codecs.push((field.name.clone(), mapped.codec));
        }
        let object = |members: Vec<String>| {
            if members.is_empty() {
                "{}".to_owned()
            } else {
                format!("{{ {} }}", members.join("; "))
            }
        };
        TsType {
            output: object(output),
            input: object(input),
            codec: Codec::Record(codecs),
        }
    }

    fn override_for(&self, schema: &str, name: &str) -> Option<String> {
        self.overrides
            .get(&QualifiedName::new(schema, name))
            .cloned()
    }

    fn basic(&self, schema: &str, name: &str, extension: Option<&str>) -> TsType {
        let text = || same("string".to_owned(), Codec::Text);
        match (extension, name) {
            (Some("vector"), "vector" | "halfvec") => {
                return TsType {
                    output: "number[]".into(),
                    input: "readonly number[]".into(),
                    codec: Codec::Vector,
                };
            }
            (Some("hstore"), "hstore") => {
                return TsType {
                    output: "Record<string, string | null>".into(),
                    input: "Readonly<Record<string, string | null>>".into(),
                    codec: Codec::Hstore,
                };
            }
            _ => {}
        }
        if schema != "pg_catalog" {
            return text();
        }
        match name {
            "bool" => same("boolean".into(), Codec::Bool),
            "int2" | "int4" | "oid" | "xid" | "cid" | "float4" | "float8" => {
                same("number".into(), Codec::Number)
            }
            "int8" => match self.int8 {
                // A string too, as `pg` reads an int8 and applications
                // keep it.
                Int8::Bigint => TsType {
                    output: "bigint".into(),
                    input: "bigint | number | string".into(),
                    codec: Codec::BigInt,
                },
                Int8::String => TsType {
                    output: "string".into(),
                    input: "string | bigint | number".into(),
                    codec: Codec::Text,
                },
                Int8::Number => TsType {
                    output: "number".into(),
                    input: "number | bigint | string".into(),
                    codec: Codec::Int8Number,
                },
            },
            // Unsigned 64-bit: only a bigint holds every value.
            "xid8" => TsType {
                output: "bigint".into(),
                input: "bigint | number".into(),
                codec: Codec::BigInt,
            },
            // Exact: a JS number would round it.
            "numeric" => TsType {
                output: "string".into(),
                input: "string | number".into(),
                codec: Codec::Text,
            },
            "json" | "jsonb" => TsType {
                output: format!("{RUNTIME_NS}.JsonValue"),
                input: "unknown".into(),
                codec: Codec::Json,
            },
            "timestamptz" => TsType {
                output: "Date".into(),
                input: "Date | string".into(),
                codec: Codec::Timestamptz,
            },
            "interval" => TsType {
                output: format!("{RUNTIME_NS}.Interval"),
                input: format!("{RUNTIME_NS}.Interval | string"),
                codec: Codec::Interval,
            },
            "bytea" => same("Uint8Array".into(), Codec::Bytea),
            "void" => same("undefined".into(), Codec::Void),
            _ => text(),
        }
    }
}

/// Whether `ty` is (an array of) an anonymous record, which PG has no
/// input for: a value of it can't be a parameter.
pub fn is_anonymous_record(ty: &Type) -> bool {
    match ty {
        Type::AnonymousRecord { .. } => true,
        Type::Basic { schema, name, .. } => schema == "pg_catalog" && name == "record",
        Type::Domain { base, .. } => is_anonymous_record(base),
        Type::Array { element, .. } => is_anonymous_record(element),
        _ => false,
    }
}

fn same(ts: String, codec: Codec) -> TsType {
    TsType {
        output: ts.clone(),
        input: ts,
        codec,
    }
}

/// `T[]`, parenthesizing a union element (or one holding a union: harmless).
fn array_of(elem: &str) -> String {
    if elem.contains('|') {
        format!("({elem})[]")
    } else {
        format!("{elem}[]")
    }
}

/// `name` as an object type's property key: bare when it is an identifier,
/// quoted otherwise.
pub fn property_key(name: &str) -> String {
    let mut chars = name.chars();
    let is_ident = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if is_ident {
        name.to_owned()
    } else {
        js_string(name)
    }
}

/// `s` as a JS string literal whose value is exactly `s` (JSON's escaping
/// is valid JS since ES2019).
pub fn js_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_ty(name: &str) -> Type {
        Type::Basic {
            schema: "pg_catalog".into(),
            name: name.into(),
            extension: None,
            typmod: None,
            collation: None,
        }
    }

    fn with<R>(int8: Int8, f: impl FnOnce(&TypeMapper) -> R) -> R {
        let overrides = HashMap::new();
        f(&TypeMapper {
            overrides: &overrides,
            int8,
        })
    }

    #[test]
    fn scalars() {
        with(Int8::Bigint, |m| {
            assert_eq!(m.map(&basic_ty("int4")).output, "number");
            assert_eq!(m.map(&basic_ty("int8")).output, "bigint");
            assert_eq!(m.map(&basic_ty("int8")).input, "bigint | number | string");
            assert_eq!(m.map(&basic_ty("numeric")).output, "string");
            assert_eq!(m.map(&basic_ty("tsvector")).output, "string");
            assert_eq!(m.map(&basic_ty("timestamptz")).codec, Codec::Timestamptz);
            assert_eq!(m.map(&basic_ty("interval")).output, "typedpg.Interval");
            assert_eq!(m.map(&basic_ty("jsonb")).output, "typedpg.JsonValue");
        });
    }

    #[test]
    fn int8_modes() {
        let int8 = |mode| with(mode, |m| m.map(&basic_ty("int8")));
        let s = int8(Int8::String);
        assert_eq!((s.output.as_str(), s.codec), ("string", Codec::Text));
        let n = int8(Int8::Number);
        assert_eq!((n.output.as_str(), n.codec), ("number", Codec::Int8Number));
        assert_eq!(n.input, "number | bigint | string");
        // xid8 is unsigned 64-bit whatever the int8 mode.
        with(Int8::Number, |m| {
            assert_eq!(m.map(&basic_ty("xid8")).output, "bigint")
        });
    }

    #[test]
    fn arrays_follow_element_nullability() {
        with(Int8::Bigint, |m| {
            let arr = |n| Type::Array {
                element: Box::new(basic_ty("text")),
                element_nullable: n,
            };
            assert_eq!(m.map(&arr(Some(false))).output, "string[]");
            assert_eq!(m.map(&arr(None)).output, "(string | null)[]");
            assert_eq!(m.map(&arr(Some(true))).output, "(string | null)[]");
            assert_eq!(m.map(&arr(Some(false))).input, "readonly (string | null)[]");
        });
    }

    #[test]
    fn ranges_and_multiranges() {
        with(Int8::Bigint, |m| {
            let range = |multirange| Type::Range {
                schema: "pg_catalog".into(),
                name: "int8range".into(),
                subtype: Box::new(basic_ty("int8")),
                typmod: None,
                multirange,
                extension: None,
            };
            let r = m.map(&range(false));
            assert_eq!(r.output, "typedpg.Range<bigint>");
            assert_eq!(r.input, "typedpg.Range<bigint | number | string>");
            assert_eq!(r.codec, Codec::Range(Box::new(Codec::BigInt)));
            let mr = m.map(&range(true));
            assert_eq!(mr.output, "typedpg.Range<bigint>[]");
            assert_eq!(mr.codec, Codec::Multirange(Box::new(Codec::BigInt)));
        });
    }

    #[test]
    fn extension_types() {
        with(Int8::Bigint, |m| {
            let ext = |ext: &str, name: &str| Type::Basic {
                schema: "public".into(),
                name: name.into(),
                extension: Some(ext.into()),
                typmod: None,
                collation: None,
            };
            assert_eq!(m.map(&ext("vector", "vector")).codec, Codec::Vector);
            assert_eq!(m.map(&ext("vector", "halfvec")).output, "number[]");
            assert_eq!(m.map(&ext("vector", "sparsevec")).output, "string");
            assert_eq!(
                m.map(&ext("hstore", "hstore")).output,
                "Record<string, string | null>"
            );
            assert_eq!(m.map(&ext("citext", "citext")).output, "string");
        });
    }

    #[test]
    fn enums_are_label_unions() {
        with(Int8::Bigint, |m| {
            let ty = Type::Enum {
                schema: "public".into(),
                name: "mood".into(),
                labels: vec!["happy".into(), "it's \"ok\"".into()],
                extension: None,
            };
            assert_eq!(m.map(&ty).output, r#""happy" | "it's \"ok\"""#);
        });
    }

    #[test]
    fn records_quote_odd_keys_and_nest() {
        with(Int8::Bigint, |m| {
            let inner = Type::AnonymousRecord {
                fields: vec![RecordField {
                    name: "f1".into(),
                    ty: basic_ty("int4"),
                    nullable: false,
                }],
            };
            let ty = Type::AnonymousRecord {
                fields: vec![
                    RecordField {
                        name: "?column?".into(),
                        ty: basic_ty("text"),
                        nullable: true,
                    },
                    RecordField {
                        name: "rs".into(),
                        ty: Type::Array {
                            element: Box::new(inner),
                            element_nullable: Some(false),
                        },
                        nullable: false,
                    },
                ],
            };
            assert_eq!(
                m.map(&ty).output,
                r#"{ "?column?": string | null; rs: { f1: number }[] }"#
            );
            let nullable_records = Type::Array {
                element: Box::new(ty.clone()),
                element_nullable: None,
            };
            assert_eq!(
                m.map(&nullable_records).output,
                r#"({ "?column?": string | null; rs: { f1: number }[] } | null)[]"#
            );
        });
    }

    #[test]
    fn overrides_keep_the_kind_codec() {
        let mut o = HashMap::new();
        o.insert(
            QualifiedName::new("public", "prefs"),
            "UserPreferences".to_owned(),
        );
        let m = TypeMapper {
            overrides: &o,
            int8: Int8::Bigint,
        };
        let ty = Type::Domain {
            schema: "public".into(),
            name: "prefs".into(),
            base: Box::new(basic_ty("jsonb")),
            extension: None,
            typmod: None,
            collation: None,
        };
        let mapped = m.map(&ty);
        assert_eq!(mapped.output, "UserPreferences");
        assert_eq!(mapped.input, "UserPreferences");
        assert_eq!(mapped.codec, Codec::Json);
        // An array of the domain is an array of the user's type.
        let arr = Type::Array {
            element: Box::new(ty),
            element_nullable: None,
        };
        assert_eq!(m.map(&arr).output, "(UserPreferences | null)[]");
    }
}
