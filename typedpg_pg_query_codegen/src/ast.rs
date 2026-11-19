//! Generation of the AST types from PostgreSQL's headers.
//!
//! This is the second half of libpg_query's `pg_query.proto` step: given a
//! [`Schema`] (see [`crate::schema`], the port of `extract_headers.rb`), it
//! assembles the messages and enums that
//! `scripts/generate_protobuf_and_funcs.rb` would have put into the `.proto`,
//! and emits them as Rust — but without a `.proto`, without `prost`, and
//! without any protobuf tags or attributes.
//!
//! The shape follows `prost-build`'s output (which is what the crate used to
//! commit) so that the rest of the workspace doesn't notice the change:
//! the same struct names, field names and field types (including the `Box`
//! and `Option` choices), the same oneof modules, the same `#[repr(i32)]`
//! enums with `as_str_name` / `from_str_name`, and the per-field enum
//! accessors that `prost-derive` used to generate. Only the `#[prost(...)]`
//! attributes, the `::prost::Message` / `Oneof` / `Enumeration` derives and
//! the protobuf tags are gone.
//!
//! The rules ported from `generate_protobuf_and_funcs.rb`:
//!
//! - `TYPE_OVERRIDES` / `OUTNAME_OVERRIDES` (skipped fields and `base_stmt`);
//! - the C-type → proto-type mapping of `generate_outmethods!`, including the
//!   enum fallthrough and the aliases from `typedefs.json` (`DistinctExpr`
//!   and `NullIfExpr` are `OpExpr` copies);
//! - the `Node` oneof, built from `nodes/nodetags.h` order plus the
//!   synthesized value/list types;
//! - the `Token` enum: `NUL`, the single-character `ASCII_*` tokens and
//!   `gram.h`'s `yytokentype`, plus `SQL_COMMENT` and `C_COMMENT` (which
//!   libpg_query gets from its grammar patch and typedpg's scanner
//!   synthesizes);
//! - the enum renumbering (a dummy zero value first, then 1, 2, …).

use std::collections::HashMap;
use std::fmt::Write;

use heck::{ToSnakeCase, ToUpperCamelCase};

use crate::schema::{Schema, StructDef};
use crate::{Field, FieldType, HEADER_FROM_HEADERS, Message, Model, Oneof};

/// The error `TryFrom<i32>` returns, standing in for `prost::UnknownEnumValue`
/// now that the enums aren't `::prost::Enumeration`.
const UNKNOWN_ENUM_VALUE: &str = r#"/// An `i32` that is not a variant of the enum it was converted to, as
/// `prost::UnknownEnumValue` was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownEnumValue(pub i32);

impl ::core::fmt::Display for UnknownEnumValue {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        write!(f, "unknown enum value: {}", self.0)
    }
}

impl ::std::error::Error for UnknownEnumValue {}
"#;

/// The fields `generate_outmethods!` drops: `Query.queryId` is deliberately
/// not printed, and `JsonTablePath.value` (a `Const`) can't be represented.
const TYPE_OVERRIDES: &[(&str, &str)] = &[("Query", "queryId"), ("JsonTablePath", "value")];

/// A message plus everything the emitter needs beyond the shared [`Model`].
pub struct Program {
    /// The shared model: the messages, in `pg_query.proto` declaration order,
    /// plus the `Node` variants.
    pub model: Model,
    /// The top-level enums, in `pg_query.proto` declaration order.
    enums: Vec<Enum>,
    /// The summary API's nested `Context` enum.
    context: Enum,
}

struct Enum {
    /// The Rust enum name.
    rust: String,
    values: Vec<EnumValue>,
}

struct EnumValue {
    /// The Rust variant name.
    rust: String,
    /// The proto value name, which `as_str_name` / `from_str_name` use.
    proto: String,
    number: i32,
}

impl Program {
    /// Build the program from the headers. `gram.h` has already been read as
    /// part of `schema` (the `../backend/parser/gram` group).
    pub fn from_schema(schema: &Schema) -> Program {
        let nodetypes = nodetype_messages(schema);
        let value_nodes = value_messages();
        let node = node_message(&nodetypes, &value_nodes);

        let mut messages = vec![parse_result(), scan_result(), node];
        messages.extend(value_nodes);
        messages.extend(nodetypes);
        messages.push(scan_token());
        messages.push(summary_result());

        let node = messages
            .iter()
            .find(|m| m.name == "Node")
            .expect("the Node message");
        let oneof = &node.oneofs[0];
        let variants = oneof
            .fields
            .iter()
            .map(|&index| {
                let field = &node.fields[index];
                let FieldType::Message(ty) = &field.ty else {
                    panic!("Node variant {} is not a message", field.proto);
                };
                (field.proto.to_upper_camel_case(), ty.clone())
            })
            .collect();

        let mut model = Model {
            messages,
            variants,
            variant_of: HashMap::new(),
        };
        model.resolve_targets();
        resolve_boxed(&mut model);

        Program {
            model,
            enums: schema_enums(schema),
            context: context_enum(),
        }
    }

    /// The generated `protobuf.rs`, without going through `rustfmt`.
    pub fn emit(&self) -> String {
        let mut s = String::from(HEADER_FROM_HEADERS);
        s.push_str(UNKNOWN_ENUM_VALUE);
        for message in &self.model.messages {
            self.emit_message(&mut s, message);
        }
        for enum_def in &self.enums {
            emit_enum(&mut s, enum_def);
        }
        s
    }

    fn emit_message(&self, s: &mut String, message: &Message) {
        writeln!(s, "#[derive({})]", self.derives(&message.rust).join(", ")).unwrap();
        writeln!(s, "pub struct {} {{", message.rust).unwrap();
        // `prost-build` emits the ordinary fields first, then the oneofs.
        for field in message.fields.iter().filter(|f| f.oneof.is_none()) {
            writeln!(s, "pub {}: {},", field.rust, rust_type(&field.ty, field)).unwrap();
        }
        for oneof in &message.oneofs {
            writeln!(
                s,
                "pub {}: ::core::option::Option<{}::{}>,",
                oneof.rust,
                message.rust.to_snake_case(),
                oneof.enum_rust
            )
            .unwrap();
        }
        s.push_str("}\n");

        if !message.oneofs.is_empty() || message.has_nested {
            writeln!(
                s,
                "/// Nested message and enum types in `{}`.",
                message.name
            )
            .unwrap();
            writeln!(s, "pub mod {} {{", message.rust.to_snake_case()).unwrap();
            for oneof in &message.oneofs {
                let members: Vec<&Field> =
                    oneof.fields.iter().map(|&i| &message.fields[i]).collect();
                let copy = members.iter().all(|f| self.is_copy_field(&message.rust, f));
                let eq = members.iter().all(|f| self.is_eq_field(&message.rust, f));
                let mut derives = String::from("Clone, ");
                if copy {
                    derives.push_str("Copy, ");
                }
                derives.push_str("PartialEq, ");
                if eq {
                    derives.push_str("Eq, Hash, ");
                }
                derives.push_str("Debug");
                writeln!(s, "#[derive({derives})]").unwrap();
                writeln!(s, "pub enum {} {{", oneof.enum_rust).unwrap();
                for field in members {
                    let FieldType::Message(ty) = &field.ty else {
                        panic!("oneof {} member is not a message", oneof.proto);
                    };
                    let ty = format!("super::{ty}");
                    if field.boxed {
                        writeln!(
                            s,
                            "{}(::std::boxed::Box<{ty}>),",
                            field.proto.to_upper_camel_case()
                        )
                        .unwrap();
                    } else {
                        writeln!(s, "{}({ty}),", field.proto.to_upper_camel_case()).unwrap();
                    }
                }
                s.push_str("}\n");
            }
            for nested in &message.nested {
                writeln!(s, "#[derive({})]", self.derives(&nested.rust).join(", ")).unwrap();
                writeln!(s, "pub struct {} {{", nested.rust).unwrap();
                for field in &nested.fields {
                    writeln!(s, "pub {}: {},", field.rust, rust_type(&field.ty, field)).unwrap();
                }
                s.push_str("}\n");
                self.emit_accessors(s, nested);
            }
            if message.has_nested {
                emit_enum(s, &self.context);
            }
            s.push_str("}\n");
        }

        self.emit_accessors(s, message);
    }

    fn emit_accessors(&self, s: &mut String, message: &Message) {
        let mut body = String::new();
        for field in &message.fields {
            let FieldType::Enum(enum_rust) = &field.ty else {
                continue;
            };
            let default = self
                .enum_default(enum_rust)
                .unwrap_or_else(|| panic!("enum {enum_rust} used by {}", field.rust));
            let ident = field.rust.strip_prefix("r#").unwrap_or(&field.rust);
            let rust = &field.rust;
            writeln!(
                body,
                "/// Returns the enum value of `{ident}`, or the default if the field is set to an invalid enum value.\n\
                 pub fn {rust}(&self) -> {enum_rust} {{\n\
                 ::core::convert::TryFrom::try_from(self.{rust}).unwrap_or({enum_rust}::{default})\n\
                 }}\n\n\
                 /// Sets `{ident}` to the provided enum value.\n\
                 pub fn set_{ident}(&mut self, value: {enum_rust}) {{\n\
                 self.{rust} = value as i32;\n\
                 }}"
            )
            .unwrap();
        }
        if !body.is_empty() {
            writeln!(s, "impl {} {{\n{body}\n}}", message.rust).unwrap();
        }
    }

    fn enum_default(&self, enum_rust: &str) -> Option<String> {
        let enum_def = self
            .enums
            .iter()
            .chain(std::iter::once(&self.context))
            .find(|e| e.rust == enum_rust)?;
        enum_def.values.first().map(|v| v.rust.clone())
    }

    /// `prost-build`'s derive selection for a message: `Clone, [Copy,]
    /// PartialEq, [Eq, Hash,]`; `Debug` and `Default` come from the derives
    /// that used to be `::prost::Message`.
    fn derives(&self, rust: &str) -> Vec<String> {
        let mut out = vec!["Clone".to_string()];
        if self.is_copy(rust) {
            out.push("Copy".to_string());
        }
        out.push("PartialEq".to_string());
        if self.is_eq(rust) {
            out.push("Eq".to_string());
            out.push("Hash".to_string());
        }
        out.push("Debug".to_string());
        out.push("Default".to_string());
        out
    }

    fn is_copy_field(&self, owner: &str, field: &Field) -> bool {
        match &field.ty {
            FieldType::Message(ty) => {
                !field.repeated && !self.reaches(ty, owner) && self.is_copy(ty)
            }
            FieldType::Map | FieldType::String => false,
            FieldType::Bool
            | FieldType::I32
            | FieldType::I64
            | FieldType::U32
            | FieldType::U64
            | FieldType::F32
            | FieldType::F64
            | FieldType::Enum(_) => !field.repeated,
        }
    }

    fn is_eq_field(&self, owner: &str, field: &Field) -> bool {
        match &field.ty {
            FieldType::Message(ty) => !field.repeated && !self.reaches(ty, owner) && self.is_eq(ty),
            FieldType::Map => false,
            FieldType::F32 | FieldType::F64 => false,
            _ => true,
        }
    }

    fn is_copy(&self, rust: &str) -> bool {
        let message = self.message(rust);
        message
            .fields
            .iter()
            .all(|field| self.is_copy_field(rust, field))
    }

    fn is_eq(&self, rust: &str) -> bool {
        let message = self.message(rust);
        message
            .fields
            .iter()
            .all(|field| self.is_eq_field(rust, field))
    }

    fn message(&self, rust: &str) -> &Message {
        self.model
            .messages
            .iter()
            .find(|m| m.rust == rust)
            .or_else(|| {
                self.model
                    .messages
                    .iter()
                    .flat_map(|m| &m.nested)
                    .find(|m| m.rust == rust)
            })
            .unwrap_or_else(|| panic!("message {rust}"))
    }

    /// Whether `from` can reach `to` through singular message fields —
    /// `prost-build`'s `MessageGraph`, which decides where a field is boxed
    /// (and therefore where a type can't be `Copy`).
    fn reaches(&self, from: &str, to: &str) -> bool {
        fn walk(program: &Program, current: &str, to: &str, seen: &mut Vec<String>) -> bool {
            if current == to {
                return true;
            }
            if seen.iter().any(|s| s == current) {
                return false;
            }
            seen.push(current.to_string());
            program.message(current).fields.iter().any(|field| {
                let FieldType::Message(ty) = &field.ty else {
                    return false;
                };
                !field.repeated && walk(program, ty, to, seen)
            })
        }
        walk(self, from, to, &mut Vec::new())
    }
}

/// Box every field whose message type can reach its own message through
/// singular fields (an infinitely sized Rust type otherwise), as
/// `prost-build`'s `MessageGraph` does.
fn resolve_boxed(model: &mut Model) {
    fn edges(messages: &[Message], from: &str, to: &str) -> bool {
        fn walk(messages: &[Message], current: &str, to: &str, seen: &mut Vec<String>) -> bool {
            if current == to {
                return true;
            }
            if seen.iter().any(|s| s == current) {
                return false;
            }
            seen.push(current.to_string());
            let message = messages
                .iter()
                .find(|m| m.rust == current)
                .unwrap_or_else(|| panic!("message {current}"));
            message.fields.iter().any(|field| {
                let FieldType::Message(ty) = &field.ty else {
                    return false;
                };
                !field.repeated && walk(messages, ty, to, seen)
            })
        }
        walk(messages, from, to, &mut Vec::new())
    }

    let mut boxed: Vec<Vec<bool>> = Vec::with_capacity(model.messages.len());
    for message in &model.messages {
        let owner = message.rust.clone();
        boxed.push(
            message
                .fields
                .iter()
                .map(|field| match &field.ty {
                    FieldType::Message(ty) => !field.repeated && edges(&model.messages, ty, &owner),
                    _ => false,
                })
                .collect(),
        );
    }
    for (message, flags) in model.messages.iter_mut().zip(boxed) {
        for (field, boxed) in message.fields.iter_mut().zip(flags) {
            field.boxed = boxed;
        }
    }
}

fn parse_result() -> Message {
    Message {
        name: "ParseResult".into(),
        rust: "ParseResult".into(),
        fields: vec![
            field("version", "version", FieldType::I32, false),
            field("stmts", "stmts", FieldType::Message("RawStmt".into()), true),
        ],
        oneofs: Vec::new(),
        nested: Vec::new(),
        has_nested: false,
    }
}

fn scan_result() -> Message {
    Message {
        name: "ScanResult".into(),
        rust: "ScanResult".into(),
        fields: vec![
            field("version", "version", FieldType::I32, false),
            field(
                "tokens",
                "tokens",
                FieldType::Message("ScanToken".into()),
                true,
            ),
        ],
        oneofs: Vec::new(),
        nested: Vec::new(),
        has_nested: false,
    }
}

/// The `Node` oneof: every AST message, in `nodes/nodetags.h` order, then the
/// synthesized value/list nodes. The `.proto` names each variant with the
/// generator's underscore rule, which is not always the type name's snake
/// case (`SQLValueFunction` → `sqlvalue_function`).
fn node_message(nodetypes: &[Message], value_nodes: &[Message]) -> Message {
    // `generate_protobuf_and_funcs.rb` appends the synthesized nodes in this
    // order, which is not their message declaration order (`List`, `OidList`,
    // `IntList` in the `.proto`, but `List`, `IntList`, `OidList` here).
    let value_order = [
        "Integer",
        "Float",
        "Boolean",
        "String",
        "BitString",
        "List",
        "IntList",
        "OidList",
        "AConst",
    ];
    let ordered: Vec<&Message> = value_order
        .iter()
        .map(|name| {
            value_nodes
                .iter()
                .find(|m| m.rust == *name)
                .unwrap_or_else(|| panic!("value message {name}"))
        })
        .collect();
    let members: Vec<&Message> = nodetypes.iter().chain(ordered).collect();
    let fields = members
        .iter()
        .map(|m| {
            let proto = ruby_underscore(&m.name);
            let mut f = field(&proto, &m.name, FieldType::Message(m.rust.clone()), false);
            f.oneof = Some(0);
            f
        })
        .collect();
    Message {
        name: "Node".into(),
        rust: "Node".into(),
        fields,
        oneofs: vec![Oneof {
            proto: "node".into(),
            rust: "node".into(),
            enum_rust: "Node".into(),
            fields: (0..members.len()).collect(),
        }],
        nested: Vec::new(),
        has_nested: false,
    }
}

/// The messages `pg_query.proto` declares for `nodes/value.h` and
/// `nodes/pg_list.h`.
fn value_messages() -> Vec<Message> {
    let mut messages = vec![
        simple_message("Integer", "ival", FieldType::I32),
        simple_message("Float", "fval", FieldType::String),
        simple_message("Boolean", "boolval", FieldType::Bool),
        simple_message("String", "sval", FieldType::String),
        simple_message("BitString", "bsval", FieldType::String),
    ];
    for name in ["List", "OidList", "IntList"] {
        messages.push(Message {
            name: name.into(),
            rust: name.into(),
            fields: vec![field(
                "items",
                "items",
                FieldType::Message("Node".into()),
                true,
            )],
            oneofs: Vec::new(),
            nested: Vec::new(),
            has_nested: false,
        });
    }
    messages.push(a_const_message());
    messages
}

fn simple_message(name: &str, proto: &str, ty: FieldType) -> Message {
    Message {
        name: name.into(),
        rust: name.into(),
        fields: vec![field(proto, proto, ty, false)],
        oneofs: Vec::new(),
        nested: Vec::new(),
        has_nested: false,
    }
}

fn a_const_message() -> Message {
    let members = [
        ("ival", "Integer"),
        ("fval", "Float"),
        ("boolval", "Boolean"),
        ("sval", "String"),
        ("bsval", "BitString"),
    ];
    let mut fields: Vec<Field> = members
        .iter()
        .map(|(proto, ty)| {
            let mut field = field(proto, proto, FieldType::Message((*ty).into()), false);
            field.oneof = Some(0);
            field
        })
        .collect();
    fields.push(field("isnull", "isnull", FieldType::Bool, false));
    fields.push(field("location", "location", FieldType::I32, false));
    Message {
        name: "A_Const".into(),
        rust: "AConst".into(),
        fields,
        oneofs: vec![Oneof {
            proto: "val".into(),
            rust: "val".into(),
            enum_rust: "Val".into(),
            fields: (0..members.len()).collect(),
        }],
        nested: Vec::new(),
        has_nested: false,
    }
}

fn scan_token() -> Message {
    Message {
        name: "ScanToken".into(),
        rust: "ScanToken".into(),
        fields: vec![
            field("start", "start", FieldType::I32, false),
            field("end", "end", FieldType::I32, false),
            field("token", "token", FieldType::Enum("Token".into()), false),
            field(
                "keyword_kind",
                "keyword_kind",
                FieldType::Enum("KeywordKind".into()),
                false,
            ),
        ],
        oneofs: Vec::new(),
        nested: Vec::new(),
        has_nested: false,
    }
}

fn summary_result() -> Message {
    let text = |proto: &str| field(proto, proto, FieldType::String, false);
    let table = Message {
        name: "Table".into(),
        rust: "Table".into(),
        fields: vec![
            text("name"),
            text("schema_name"),
            text("table_name"),
            field(
                "context",
                "context",
                FieldType::Enum("Context".into()),
                false,
            ),
        ],
        oneofs: Vec::new(),
        nested: Vec::new(),
        has_nested: false,
    };
    let function = Message {
        name: "Function".into(),
        rust: "Function".into(),
        fields: vec![
            text("name"),
            text("function_name"),
            text("schema_name"),
            field(
                "context",
                "context",
                FieldType::Enum("Context".into()),
                false,
            ),
        ],
        oneofs: Vec::new(),
        nested: Vec::new(),
        has_nested: false,
    };
    let filter_column = Message {
        name: "FilterColumn".into(),
        rust: "FilterColumn".into(),
        fields: vec![text("schema_name"), text("table_name"), text("column")],
        oneofs: Vec::new(),
        nested: Vec::new(),
        has_nested: false,
    };
    Message {
        name: "SummaryResult".into(),
        rust: "SummaryResult".into(),
        fields: vec![
            field(
                "tables",
                "tables",
                FieldType::Message("summary_result::Table".into()),
                true,
            ),
            field("aliases", "aliases", FieldType::Map, false),
            field("cte_names", "cte_names", FieldType::String, true),
            field(
                "functions",
                "functions",
                FieldType::Message("summary_result::Function".into()),
                true,
            ),
            field(
                "filter_columns",
                "filter_columns",
                FieldType::Message("summary_result::FilterColumn".into()),
                true,
            ),
            field(
                "statement_types",
                "statement_types",
                FieldType::String,
                true,
            ),
            text("truncated_query"),
        ],
        oneofs: Vec::new(),
        nested: vec![table, function, filter_column],
        has_nested: true,
    }
}

fn context_enum() -> Enum {
    Enum {
        rust: "Context".into(),
        values: ["None", "Select", "DML", "DDL", "Call"]
            .iter()
            .enumerate()
            .map(|(number, proto)| EnumValue {
                rust: variant_name("Context", proto),
                proto: (*proto).to_string(),
                number: number as i32,
            })
            .collect(),
    }
}

fn field(proto: &str, json: &str, ty: FieldType, repeated: bool) -> Field {
    Field {
        proto: proto.to_string(),
        json: json.to_string(),
        rust: crate::field_ident(proto),
        ty,
        repeated,
        boxed: false,
        oneof: None,
        target: None,
    }
}

/// One message per node tag that has a struct in `parsenodes.h` or
/// `primnodes.h` (or is a typedef alias of one, like `DistinctExpr`).
fn nodetype_messages(schema: &Schema) -> Vec<Message> {
    let mut messages = Vec::new();
    for nodetype in &schema.nodetypes {
        if matches!(nodetype.as_str(), "Value" | "Const") {
            continue;
        }
        let Some((struct_def, name)) = node_struct(schema, nodetype) else {
            continue;
        };
        let rust = nodetype.to_upper_camel_case();
        let mut fields = Vec::new();
        for field_def in &struct_def.fields {
            let (Some(cname), Some(ctype)) = (&field_def.name, &field_def.c_type) else {
                continue;
            };
            if ctype == "NodeTag" {
                continue;
            }
            if TYPE_OVERRIDES
                .iter()
                .any(|(ty, f)| *ty == name && *f == cname)
            {
                continue;
            }
            let proto = proto_field_name(&name, cname);
            let Some((ty, repeated)) = map_c_type(schema, ctype) else {
                // `generate_outmethods!` prints `ERR` and emits no field.
                continue;
            };
            fields.push(field(&proto, cname, ty, repeated));
        }
        messages.push(Message {
            name,
            rust,
            fields,
            oneofs: Vec::new(),
            nested: Vec::new(),
            has_nested: false,
        });
    }
    messages
}

/// The struct backing node tag `nodetype`: the struct itself, or the struct of
/// the typedef it aliases (with the alias's name).
fn node_struct(schema: &Schema, nodetype: &str) -> Option<(StructDef, String)> {
    for group in ["nodes/parsenodes", "nodes/primnodes"] {
        if let Some(struct_def) = schema.struct_def(group, nodetype) {
            return Some((struct_def.clone(), nodetype.to_string()));
        }
    }
    let typedef = schema.typedef(nodetype)?;
    if typedef.source.contains(' ') || typedef.source.contains('*') {
        return None;
    }
    for group in ["nodes/parsenodes", "nodes/primnodes"] {
        if let Some(struct_def) = schema.struct_def(group, &typedef.source) {
            return Some((struct_def.clone(), nodetype.to_string()));
        }
    }
    None
}

/// `generate_outmethods!`'s type mapping: the proto field type and whether the
/// field repeats.
fn map_c_type(schema: &Schema, c_type: &str) -> Option<(FieldType, bool)> {
    let scalar = |ty| Some((ty, false));
    match c_type {
        "char" | "char*" => scalar(FieldType::String),
        "bool" => scalar(FieldType::Bool),
        "long" => scalar(FieldType::I64),
        "int" | "int16" | "int32" | "AttrNumber" | "ParseLoc" => scalar(FieldType::I32),
        "uint" | "uint16" | "uint32" | "Index" | "bits32" | "Oid" | "SubTransactionId"
        | "RelFileNumber" => scalar(FieldType::U32),
        "uint64" | "AclMode" => scalar(FieldType::U64),
        "float" | "double" | "Cost" | "Cardinality" | "Selectivity" => scalar(FieldType::F64),
        "Bitmapset*" | "Relids" => Some((FieldType::U64, true)),
        "Value" | "Value*" | "Node*" | "Node" | "Expr*" | "Expr" | "JsonTablePlan*"
        | "JsonTablePlan" => scalar(FieldType::Message("Node".into())),
        "List*" => Some((FieldType::Message("Node".into()), true)),
        "CreateStmt" => scalar(FieldType::Message("CreateStmt".into())),
        other => {
            if let Some(inner) = other.strip_suffix('*') {
                if schema.is_nodetype(inner) {
                    scalar(FieldType::Message(inner.to_upper_camel_case()))
                } else {
                    // `generate_outmethods!` prints `ERR` and emits no field.
                    None
                }
            } else {
                scalar(FieldType::Enum(other.to_upper_camel_case()))
            }
        }
    }
}

/// The `.proto` field name for node `node`'s C field `cname`:
/// `OUTNAME_OVERRIDES` (`CreateForeignTableStmt.base` → `base_stmt`) then
/// `Generators#underscore`.
fn proto_field_name(node: &str, cname: &str) -> String {
    if node == "CreateForeignTableStmt" && cname == "base" {
        "base_stmt".to_string()
    } else {
        ruby_underscore(cname)
    }
}

/// `Generators#underscore` (the ActiveSupport-style one in
/// `generate_protobuf_and_funcs.rb`), which names proto fields and `Node`
/// oneof members.
fn ruby_underscore(word: &str) -> String {
    use std::sync::LazyLock;
    static FIRST: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^([A-Z\d])([A-Z][a-z])").unwrap());
    static SECOND: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"([A-Z\d]+[a-z]+)([A-Z][a-z])").unwrap());
    static THIRD: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"([a-z\d])([A-Z])").unwrap());

    if !word.chars().any(|c| c.is_ascii_uppercase() || c == '-') && !word.contains("::") {
        return word.to_string();
    }
    let mut out = word.replace("::", "/");
    out = FIRST.replace_all(&out, "${1}__${2}").into_owned();
    out = SECOND.replace_all(&out, "${1}_${2}").into_owned();
    out = THIRD.replace_all(&out, "${1}_${2}").into_owned();
    out.replace('-', "_").to_lowercase()
}

/// The enums, in `pg_query.proto` declaration order: the header enums of the
/// five groups the generator walks, then `KeywordKind` and `Token`.
fn schema_enums(schema: &Schema) -> Vec<Enum> {
    let mut enums = Vec::new();
    for group in [
        "nodes/parsenodes",
        "nodes/primnodes",
        "nodes/nodes",
        "nodes/lockoptions",
        "access/cmptype",
    ] {
        for enum_def in schema.enums(group) {
            let Some(name) = &enum_def.name else {
                continue;
            };
            if name == "NodeTag" {
                continue;
            }
            let rust = name.to_upper_camel_case();
            let undefined = format!("{}_UNDEFINED", ruby_underscore(name).to_uppercase());
            let mut values = vec![EnumValue {
                rust: variant_name(&rust, &undefined),
                proto: undefined,
                number: 0,
            }];
            for (index, (proto, _)) in enum_def.resolved_values().iter().enumerate() {
                values.push(EnumValue {
                    rust: variant_name(&rust, proto),
                    proto: proto.clone(),
                    number: i32::try_from(index + 1).expect("enum value count"),
                });
            }
            enums.push(Enum { rust, values });
        }
    }

    enums.push(Enum {
        rust: "KeywordKind".into(),
        values: [
            "NO_KEYWORD",
            "UNRESERVED_KEYWORD",
            "COL_NAME_KEYWORD",
            "TYPE_FUNC_NAME_KEYWORD",
            "RESERVED_KEYWORD",
        ]
        .iter()
        .enumerate()
        .map(|(number, proto)| EnumValue {
            rust: variant_name("KeywordKind", proto),
            proto: (*proto).to_string(),
            number: number as i32,
        })
        .collect(),
    });
    enums.push(token_enum(schema));
    enums
}

/// The scan tokens: `NUL`, the single-character ASCII tokens, `gram.h`'s
/// `yytokentype`, and the two comment tokens libpg_query adds to the grammar.
///
/// `SQL_COMMENT` / `C_COMMENT` keep the numbers they had in libpg_query's
/// proto (275, 276) when those are free; `gram.h` numbers its tokens from
/// 258, so 275 and 276 are taken (`ABORT_P`, `ABSENT`). They are therefore
/// numbered after the highest `gram.h` token, and the named tokens keep
/// `gram.h`'s numbering — two lower than libpg_query's, whose grammar patch
/// inserts them in the middle.
fn token_enum(schema: &Schema) -> Enum {
    const ASCII: &[i32] = &[
        36, 37, 40, 41, 42, 43, 44, 45, 46, 47, 58, 59, 60, 61, 62, 63, 91, 92, 93, 94,
    ];
    let mut values = vec![EnumValue {
        rust: "Nul".into(),
        proto: "NUL".into(),
        number: 0,
    }];
    for &c in ASCII {
        values.push(EnumValue {
            rust: variant_name("Token", &format!("ASCII_{c}")),
            proto: format!("ASCII_{c}"),
            number: c,
        });
    }

    let gram = schema
        .enum_def("../backend/parser/gram", "yytokentype")
        .expect("yytokentype in gram.h");
    let mut highest = 0;
    for (name, number) in gram.resolved_values() {
        // Bison's own tokens (`YYEOF`, `YYerror`, …); libpg_query's older
        // Bison doesn't emit them, and they aren't scan tokens.
        if name.starts_with("YY") {
            continue;
        }
        let number = i32::try_from(number).expect("grammar token number");
        highest = highest.max(number);
        values.push(EnumValue {
            rust: variant_name("Token", &name),
            proto: name,
            number,
        });
    }
    // libpg_query's grammar patch declares them after `NOT_EQUALS`, so put
    // them in the same place; their numbers, though, are the `gram.h`-based
    // ones decided above.
    let position = values
        .iter()
        .position(|v| v.proto == "NOT_EQUALS")
        .expect("NOT_EQUALS in gram.h")
        + 1;
    for (offset, name) in ["SQL_COMMENT", "C_COMMENT"].iter().enumerate() {
        values.insert(
            position + offset,
            EnumValue {
                rust: variant_name("Token", name),
                proto: (*name).to_string(),
                number: highest + 1 + offset as i32,
            },
        );
    }
    Enum {
        rust: "Token".into(),
        values,
    }
}

/// `prost_build::ident::strip_enum_prefix` on `to_upper_camel`.
fn variant_name(enum_rust: &str, proto: &str) -> String {
    let name = proto.to_upper_camel_case();
    let stripped = name.strip_prefix(enum_rust).unwrap_or(&name);
    let stripped = if stripped
        .chars()
        .next()
        .map(char::is_uppercase)
        .unwrap_or(false)
    {
        stripped
    } else {
        &name
    };
    sanitize_ident(stripped)
}

/// `prost_build::ident::sanitize_identifier`.
fn sanitize_ident(ident: &str) -> String {
    match ident {
        "as" | "break" | "const" | "continue" | "else" | "enum" | "false" | "fn" | "for" | "if"
        | "impl" | "in" | "let" | "loop" | "match" | "mod" | "move" | "mut" | "pub" | "ref"
        | "return" | "static" | "struct" | "trait" | "true" | "type" | "unsafe" | "use"
        | "where" | "while" | "dyn" | "abstract" | "become" | "box" | "do" | "final" | "macro"
        | "override" | "priv" | "typeof" | "unsized" | "virtual" | "yield" | "async" | "await"
        | "try" | "gen" => format!("r#{ident}"),
        "_" | "super" | "self" | "Self" | "extern" | "crate" => format!("{ident}_"),
        s if s.starts_with(|c: char| c.is_numeric()) => format!("_{ident}"),
        _ => ident.to_string(),
    }
}

/// The Rust type of a field.
fn rust_type(ty: &FieldType, field: &Field) -> String {
    let inner = match ty {
        FieldType::Bool => "bool".to_string(),
        FieldType::I32 => "i32".to_string(),
        FieldType::I64 => "i64".to_string(),
        FieldType::U32 => "u32".to_string(),
        FieldType::U64 => "u64".to_string(),
        FieldType::F32 => "f32".to_string(),
        FieldType::F64 => "f64".to_string(),
        FieldType::String => "::std::string::String".to_string(),
        // Enum fields are stored as their `i32` discriminant (`prost`'s
        // representation); the accessors convert to the enum.
        FieldType::Enum(_) => "i32".to_string(),
        FieldType::Message(name) => {
            if field.boxed {
                format!("::std::boxed::Box<{name}>")
            } else {
                name.clone()
            }
        }
        FieldType::Map => {
            return "::std::collections::HashMap<::std::string::String, ::std::string::String>"
                .to_string();
        }
    };
    if field.repeated {
        let item = match ty {
            FieldType::Message(name) => name.clone(),
            _ => inner,
        };
        format!("::std::vec::Vec<{item}>")
    } else if matches!(ty, FieldType::Message(_)) {
        format!("::core::option::Option<{inner}>")
    } else {
        inner
    }
}

fn emit_enum(s: &mut String, enum_def: &Enum) {
    s.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]\n");
    s.push_str("#[repr(i32)]\n");
    writeln!(s, "pub enum {} {{", enum_def.rust).unwrap();
    for value in &enum_def.values {
        writeln!(s, "{} = {},", value.rust, value.number).unwrap();
    }
    s.push_str("}\n");

    writeln!(s, "impl {} {{", enum_def.rust).unwrap();
    s.push_str(
        "/// String value of the enum field names used in the ProtoBuf definition.\n\
         ///\n\
         /// The values are not transformed in any way and thus are considered stable\n\
         /// (if the ProtoBuf definition does not change) and safe for programmatic use.\n\
         pub fn as_str_name(&self) -> &'static str {\n\
         match self {\n",
    );
    for value in &enum_def.values {
        writeln!(s, "Self::{} => {:?},", value.rust, value.proto).unwrap();
    }
    s.push_str(
        "}\n\
         }\n\n\
         /// Creates an enum from field names used in the ProtoBuf definition.\n\
         pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {\n\
         match value {\n",
    );
    for value in &enum_def.values {
        writeln!(s, "{:?} => Some(Self::{}),", value.proto, value.rust).unwrap();
    }
    s.push_str("_ => None,\n}\n}\n}\n");

    let first = enum_def
        .values
        .first()
        .expect("enums have at least one value");
    writeln!(
        s,
        "impl ::core::default::Default for {} {{\nfn default() -> {} {{\n{}::{}\n}}\n}}",
        enum_def.rust, enum_def.rust, enum_def.rust, first.rust
    )
    .unwrap();
    writeln!(
        s,
        "impl ::core::convert::From<{}> for i32 {{\nfn from(value: {}) -> i32 {{\nvalue as i32\n}}\n}}",
        enum_def.rust, enum_def.rust
    )
    .unwrap();
    writeln!(
        s,
        "impl ::core::convert::TryFrom<i32> for {} {{\ntype Error = crate::protobuf::UnknownEnumValue;\n\nfn try_from(value: i32) -> ::core::result::Result<{}, Self::Error> {{\nmatch value {{",
        enum_def.rust, enum_def.rust
    )
    .unwrap();
    for value in &enum_def.values {
        writeln!(
            s,
            "{} => ::core::result::Result::Ok({}::{}),",
            value.number, enum_def.rust, value.rust
        )
        .unwrap();
    }
    writeln!(
        s,
        "_ => ::core::result::Result::Err(crate::protobuf::UnknownEnumValue(value)),\n}}\n}}\n}}"
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use heck::{ToSnakeCase, ToUpperCamelCase};

    use super::{proto_field_name, ruby_underscore};
    use crate::{field_ident, type_ident};

    #[test]
    fn underscore_mirrors_the_ruby_generator() {
        // `Generators#underscore` (ActiveSupport-style), not `heck`.
        assert_eq!(ruby_underscore("A_Const"), "a_const");
        assert_eq!(ruby_underscore("SQLValueFunction"), "sqlvalue_function");
        assert_eq!(ruby_underscore("CTESearchClause"), "ctesearch_clause");
        assert_eq!(ruby_underscore("base"), "base");
        assert_eq!(ruby_underscore("queryId"), "query_id");
    }

    #[test]
    fn tricky_type_names() {
        // `A_Const` becomes `AConst` (and its proto field `a_const`).
        assert_eq!(type_ident("A_Const"), "AConst");
        assert_eq!(ruby_underscore("A_Const"), "a_const");
        assert_eq!(ruby_underscore("A_Const").to_upper_camel_case(), "AConst");

        assert_eq!(type_ident("PLAssignStmt"), "PlAssignStmt");
        assert_eq!(ruby_underscore("PLAssignStmt"), "plassign_stmt");
        assert_eq!(field_ident("plassign_stmt"), "plassign_stmt");
        assert_eq!(
            ruby_underscore("PLAssignStmt").to_upper_camel_case(),
            "PlassignStmt"
        );

        assert_eq!(type_ident("XmlSerialize"), "XmlSerialize");
        assert_eq!(ruby_underscore("XmlSerialize"), "xml_serialize");
        assert_eq!(field_ident("xml_serialize"), "xml_serialize");

        assert_eq!(type_ident("CTECycleClause"), "CteCycleClause");
        assert_eq!(ruby_underscore("CTECycleClause"), "ctecycle_clause");
        assert_eq!(
            ruby_underscore("CTECycleClause").to_upper_camel_case(),
            "CtecycleClause"
        );
    }

    #[test]
    fn field_name_overrides() {
        // `OUTNAME_OVERRIDES[['CreateForeignTableStmt', 'base']]`.
        assert_eq!(
            proto_field_name("CreateForeignTableStmt", "base"),
            "base_stmt"
        );
        assert_eq!(
            field_ident(&proto_field_name("CreateForeignTableStmt", "base")),
            "base_stmt"
        );
        // Other nodes' `base` keeps its name.
        assert_eq!(proto_field_name("OtherStmt", "base"), "base");
        // `queryId` is a plain camelCase field elsewhere.
        assert_eq!(proto_field_name("OtherStmt", "queryId"), "query_id");
        // `ruby_underscore` and `heck` agree on a plain snake_case name.
        assert_eq!("base_stmt".to_snake_case(), "base_stmt");
    }
}
