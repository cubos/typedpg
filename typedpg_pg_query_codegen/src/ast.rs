//! Generation of the AST types from PostgreSQL's headers.
//!
//! Given a [`Schema`] (see [`crate::schema`], the port of libpg_query's
//! `extract_headers.rb`), this assembles the node types and enums the parser
//! produces and emits them as Rust:
//!
//! - one struct per node type of `parsenodes.h` / `primnodes.h` (and per
//!   typedef alias of one: `DistinctExpr` and `NullIfExpr` are `OpExpr`
//!   copies), plus the value and list nodes of `nodes/value.h` and
//!   `nodes/pg_list.h`, which the generator defines itself;
//! - `Node`, an enum with one variant per node type, each boxed in a
//!   `NodeBox` so a `Node` stays two words. Its variants are in
//!   `nodes/nodetags.h` order, then the value and list nodes;
//! - PostgreSQL's enums, with PostgreSQL's own values (a field holds the
//!   enum, and `Default` is the variant whose value is 0, what `makeNode`'s
//!   zeroing gives);
//! - `Token` and `KeywordKind`, the scanner's: `NUL`, the single-character
//!   `ASCII_*` tokens and `gram.h`'s `yytokentype`, plus `SQL_COMMENT` and
//!   `C_COMMENT`, which the scanner wrapper synthesizes (numbered after the
//!   highest `gram.h` token).
//!
//! A field's type follows its C type: a `Node *` (or `Expr *`) is an
//! `Option<Node>`, a `List *` a `Vec<Option<Node>>` (PostgreSQL puts NULL
//! cells in some lists: `DISTINCT` is `list_make1(NIL)`, an operator's
//! missing operand `NONE` is a NULL type), a typed node pointer (`RangeVar *`)
//! an `Option<NodeBox<RangeVar>>`, an embedded node struct
//! (`CreateForeignTableStmt.base`) the struct itself, a `Bitmapset *` the
//! `Vec<u64>` of its members. The rules kept from libpg_query's
//! node generator (`scripts/generate_*_and_funcs.rb`), which decided which
//! fields the AST has:
//!
//! - `TYPE_OVERRIDES` / `OUTNAME_OVERRIDES` (skipped fields and `base_stmt`);
//! - the C-type mapping of `generate_outmethods!`, including the enum
//!   fallthrough and the aliases from `typedefs.json`;
//! - the abstract `Expr xpr` header every expression node embeds is not a
//!   field (it holds nothing but the node tag).

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use heck::ToUpperCamelCase;

use crate::schema::{Schema, StructDef};
use crate::{Field, FieldType, HEADER_FROM_HEADERS, Message, Model, Oneof};

/// The fields `generate_outmethods!` drops: `Query.queryId` is deliberately
/// not printed, and `JsonTablePath.value` (a `Const`) can't be represented.
const TYPE_OVERRIDES: &[(&str, &str)] = &[("Query", "queryId"), ("JsonTablePath", "value")];

/// The value and list nodes, in the order they follow the node tags among
/// the `Node` variants.
const VALUE_ORDER: &[&str] = &[
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

/// The AST, ready to emit.
pub struct Program {
    /// The shared model: the types, plus the `Node` variants.
    pub model: Model,
    /// The enums, in emission order.
    pub(crate) enums: Vec<Enum>,
}

pub(crate) struct Enum {
    /// The C name (`CmdType`), or the scanner's own name for `Token` and
    /// `KeywordKind`.
    pub c_name: String,
    /// The Rust enum name.
    pub rust: String,
    pub values: Vec<EnumValue>,
}

pub(crate) struct EnumValue {
    /// The Rust variant name.
    pub rust: String,
    /// The C enumerator (`CMD_SELECT`), which `c_name()` returns.
    pub c: String,
    /// PostgreSQL's value.
    pub number: i64,
}

impl Program {
    /// Build the program from the headers. `gram.h` has already been read as
    /// part of `schema` (the `../backend/parser/gram` group).
    pub fn from_schema(schema: &Schema) -> Program {
        let nodetypes = nodetype_messages(schema);
        let value_nodes = value_messages();

        let mut variants: Vec<(String, String)> = nodetypes
            .iter()
            .map(|m| (m.rust.clone(), m.rust.clone()))
            .collect();
        variants.extend(VALUE_ORDER.iter().map(|v| (v.to_string(), v.to_string())));

        let mut messages = vec![parse_result(), scan_result()];
        messages.extend(value_nodes);
        messages.extend(nodetypes);
        messages.push(scan_token());

        let mut model = Model {
            messages,
            variants,
            variant_of: HashMap::new(),
        };
        model.resolve_targets();

        Program {
            model,
            enums: schema_enums(schema),
        }
    }

    /// The Rust names of the enums.
    pub fn enum_names(&self) -> Vec<String> {
        self.enums.iter().map(|e| e.rust.clone()).collect()
    }

    pub(crate) fn enum_by_c_name(&self, c_name: &str) -> Option<&Enum> {
        self.enums.iter().find(|e| e.c_name == c_name)
    }

    /// The generated `ast.rs`, without going through `rustfmt`.
    pub fn emit(&self) -> String {
        let traits = Traits::of(&self.model);
        let mut s = String::from(HEADER_FROM_HEADERS);
        s.push_str(
            "//! The AST: PostgreSQL's raw parse tree nodes, generated from its headers.\n\n\
             use crate::NodeBox;\n\n",
        );
        for message in &self.model.messages {
            emit_message(&mut s, message, &traits);
        }
        emit_node(&mut s, &self.model, &traits);
        for enum_def in &self.enums {
            emit_enum(&mut s, enum_def);
        }
        s
    }
}

/// Which types can derive `Copy`, and which `Eq` and `Hash`.
struct Traits {
    copy: HashSet<String>,
    eq: HashSet<String>,
}

impl Traits {
    /// The greatest fixed points: a type is `Copy` (`Eq`) unless one of its
    /// fields is not, a type recursion notwithstanding. `Node` is `Eq` when
    /// every variant is, and never `Copy`.
    fn of(model: &Model) -> Traits {
        let mut copy: HashSet<String> = model.messages.iter().map(|m| m.rust.clone()).collect();
        let mut eq = copy.clone();
        eq.insert("Node".into());
        loop {
            let mut changed = false;
            for m in &model.messages {
                if copy.contains(&m.rust) && !m.fields.iter().all(|f| field_copy(f, &copy)) {
                    copy.remove(&m.rust);
                    changed = true;
                }
                if eq.contains(&m.rust) && !m.fields.iter().all(|f| field_eq(f, &eq)) {
                    eq.remove(&m.rust);
                    changed = true;
                }
            }
            if eq.contains("Node") && !model.variants.iter().all(|(_, ty)| eq.contains(ty)) {
                eq.remove("Node");
                changed = true;
            }
            if !changed {
                return Traits { copy, eq };
            }
        }
    }

    fn derives(&self, ty: &str, default: bool) -> String {
        let mut out = vec!["Clone"];
        if self.copy.contains(ty) {
            out.push("Copy");
        }
        out.push("Debug");
        if default {
            out.push("Default");
        }
        out.push("PartialEq");
        if self.eq.contains(ty) {
            out.push("Eq");
            out.push("Hash");
        }
        out.push("serde::Serialize");
        out.push("serde::Deserialize");
        out.join(", ")
    }
}

fn field_copy(field: &Field, copy: &HashSet<String>) -> bool {
    if field.repeated {
        return false;
    }
    match &field.ty {
        FieldType::Bool
        | FieldType::I32
        | FieldType::I64
        | FieldType::U32
        | FieldType::U64
        | FieldType::F64
        | FieldType::Enum(_) => true,
        FieldType::String | FieldType::Node => false,
        FieldType::Message(ty) => (field.embedded || field.oneof.is_some()) && copy.contains(ty),
    }
}

fn field_eq(field: &Field, eq: &HashSet<String>) -> bool {
    match &field.ty {
        FieldType::F64 => false,
        FieldType::Node => eq.contains("Node"),
        FieldType::Message(ty) => eq.contains(ty),
        _ => true,
    }
}

fn emit_message(s: &mut String, message: &Message, traits: &Traits) {
    writeln!(s, "#[derive({})]", traits.derives(&message.rust, true)).unwrap();
    writeln!(s, "pub struct {} {{", message.rust).unwrap();
    for field in message.fields.iter().filter(|f| f.oneof.is_none()) {
        writeln!(s, "pub {}: {},", field.rust, rust_type(field)).unwrap();
    }
    for oneof in &message.oneofs {
        writeln!(s, "pub {}: Option<{}>,", oneof.rust, oneof.enum_rust).unwrap();
    }
    s.push_str("}\n\n");

    for oneof in &message.oneofs {
        let members: Vec<&Field> = oneof.fields.iter().map(|&i| &message.fields[i]).collect();
        let copy = members.iter().all(|f| field_copy(f, &traits.copy));
        let eq = members.iter().all(|f| field_eq(f, &traits.eq));
        let mut derives = String::from("Clone, ");
        if copy {
            derives.push_str("Copy, ");
        }
        derives.push_str("Debug, PartialEq, ");
        if eq {
            derives.push_str("Eq, Hash, ");
        }
        derives.push_str("serde::Serialize, serde::Deserialize");
        writeln!(s, "/// The value of [`{}::{}`].", message.rust, oneof.rust).unwrap();
        writeln!(s, "#[derive({derives})]").unwrap();
        writeln!(s, "pub enum {} {{", oneof.enum_rust).unwrap();
        for field in members {
            let ty = Message::oneof_variant(field);
            writeln!(s, "{ty}({ty}),").unwrap();
        }
        s.push_str("}\n\n");
    }
}

/// The `Node` enum: one boxed variant per node type.
fn emit_node(s: &mut String, model: &Model, traits: &Traits) {
    s.push_str(
        "/// Any node: what a `Node *` points to, by kind. Every variant is boxed,\n\
         /// so a `Node` is two words.\n",
    );
    writeln!(s, "#[derive({})]", traits.derives("Node", false)).unwrap();
    s.push_str("pub enum Node {\n");
    for (variant, ty) in &model.variants {
        writeln!(s, "{variant}(NodeBox<{ty}>),").unwrap();
    }
    s.push_str("}\n\n");
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
    }
}

/// The value and list nodes of `nodes/value.h` and `nodes/pg_list.h`.
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
            fields: vec![field("items", "items", FieldType::Node, true)],
            oneofs: Vec::new(),
        });
    }
    messages.push(a_const_message());
    messages
}

fn simple_message(name: &str, c_name: &str, ty: FieldType) -> Message {
    Message {
        name: name.into(),
        rust: name.into(),
        fields: vec![field(c_name, c_name, ty, false)],
        oneofs: Vec::new(),
    }
}

/// `A_Const`: `isnull`, `location`, and the value, one of the value nodes
/// (`None` for a NULL constant).
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
        .map(|(name, ty)| {
            let mut field = field(name, name, FieldType::Message((*ty).into()), false);
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
            rust: "val".into(),
            enum_rust: "AConstValue".into(),
            fields: (0..members.len()).collect(),
        }],
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
    }
}

fn field(name: &str, c_name: &str, ty: FieldType, repeated: bool) -> Field {
    Field {
        c_name: c_name.to_string(),
        rust: crate::field_ident(name),
        ty,
        repeated,
        embedded: false,
        oneof: None,
        target: None,
    }
}

/// One type per node tag that has a struct in `parsenodes.h` or
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
            let Some(mapped) = map_c_type(schema, ctype) else {
                // `generate_outmethods!` prints `ERR` and emits no field.
                continue;
            };
            let mut f = field(&field_name(&name, cname), cname, mapped.ty, mapped.repeated);
            f.embedded = mapped.embedded;
            fields.push(f);
        }
        messages.push(Message {
            name,
            rust,
            fields,
            oneofs: Vec::new(),
        });
    }
    messages
}

/// The struct backing node tag `nodetype`: the struct itself, or the struct of
/// the typedef it aliases (with the alias's name).
pub(crate) fn node_struct(schema: &Schema, nodetype: &str) -> Option<(StructDef, String)> {
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

/// What a C field type becomes.
struct Mapped {
    ty: FieldType,
    repeated: bool,
    embedded: bool,
}

/// `generate_outmethods!`'s type mapping; `None` for a field the AST doesn't
/// have.
fn map_c_type(schema: &Schema, c_type: &str) -> Option<Mapped> {
    let scalar = |ty| {
        Some(Mapped {
            ty,
            repeated: false,
            embedded: false,
        })
    };
    match c_type {
        "char" | "char*" => scalar(FieldType::String),
        "bool" => scalar(FieldType::Bool),
        "long" => scalar(FieldType::I64),
        "int" | "int16" | "int32" | "AttrNumber" | "ParseLoc" => scalar(FieldType::I32),
        "uint" | "uint16" | "uint32" | "Index" | "bits32" | "Oid" | "SubTransactionId"
        | "RelFileNumber" => scalar(FieldType::U32),
        "uint64" | "AclMode" => scalar(FieldType::U64),
        "float" | "double" | "Cost" | "Cardinality" | "Selectivity" => scalar(FieldType::F64),
        "Bitmapset*" | "Relids" => Some(Mapped {
            ty: FieldType::U64,
            repeated: true,
            embedded: false,
        }),
        "Value*" | "Node*" | "Expr*" | "JsonTablePlan*" => scalar(FieldType::Node),
        // An abstract node embedded by value: the `Expr xpr` (or
        // `JsonTablePlan plan`) header of a node, which holds nothing but
        // the node tag.
        "Value" | "Node" | "Expr" | "JsonTablePlan" => None,
        "List*" => Some(Mapped {
            ty: FieldType::Node,
            repeated: true,
            embedded: false,
        }),
        "CreateStmt" => Some(Mapped {
            ty: FieldType::Message("CreateStmt".into()),
            repeated: false,
            embedded: true,
        }),
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

/// The field name for node `node`'s C field `cname`: `OUTNAME_OVERRIDES`
/// (`CreateForeignTableStmt.base` → `base_stmt`) then `Generators#underscore`.
fn field_name(node: &str, cname: &str) -> String {
    if node == "CreateForeignTableStmt" && cname == "base" {
        "base_stmt".to_string()
    } else {
        ruby_underscore(cname)
    }
}

/// `Generators#underscore` (the ActiveSupport-style one in
/// libpg_query's node generator), which names fields.
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

/// The enums: those of the five header groups the AST's fields use, then
/// `KeywordKind` and `Token`.
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
            // bindgen's values, where it has them, are the compiler's; the
            // header parse's must then agree, unless it couldn't evaluate an
            // initializer.
            let opaque = enum_def.has_opaque_values();
            let values = enum_def
                .resolved_values()
                .into_iter()
                .map(|(c, parsed)| {
                    let number = match schema.bindgen_values.get(&format!("{name}_{c}")) {
                        Some(&evaluated) => {
                            assert!(
                                opaque || evaluated == parsed,
                                "{name}.{c} is {evaluated}, not {parsed}"
                            );
                            evaluated
                        }
                        None => {
                            assert!(
                                !opaque,
                                "{name}.{c}: an initializer the headers can't evaluate"
                            );
                            parsed
                        }
                    };
                    EnumValue {
                        rust: variant_name(&rust, &c),
                        c,
                        number,
                    }
                })
                .collect();
            enums.push(Enum {
                c_name: name.clone(),
                rust,
                values,
            });
        }
    }

    enums.push(Enum {
        c_name: "KeywordKind".into(),
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
        .map(|(number, c)| EnumValue {
            rust: variant_name("KeywordKind", c),
            c: (*c).to_string(),
            number: number as i64,
        })
        .collect(),
    });
    enums.push(token_enum(schema));
    enums
}

/// The scan tokens: `NUL`, the single-character ASCII tokens, `gram.h`'s
/// `yytokentype`, and the two comment tokens the scanner wrapper synthesizes,
/// numbered after the highest `gram.h` token (and declared after
/// `NOT_EQUALS`, where libpg_query's grammar patch put them).
fn token_enum(schema: &Schema) -> Enum {
    const ASCII: &[i64] = &[
        36, 37, 40, 41, 42, 43, 44, 45, 46, 47, 58, 59, 60, 61, 62, 63, 91, 92, 93, 94,
    ];
    let mut values = vec![EnumValue {
        rust: "Nul".into(),
        c: "NUL".into(),
        number: 0,
    }];
    for &c in ASCII {
        values.push(EnumValue {
            rust: variant_name("Token", &format!("ASCII_{c}")),
            c: format!("ASCII_{c}"),
            number: c,
        });
    }

    let gram = schema
        .enum_def("../backend/parser/gram", "yytokentype")
        .expect("yytokentype in gram.h");
    let mut highest = 0;
    for (name, number) in gram.resolved_values() {
        // Bison's own tokens (`YYEOF`, `YYerror`, …) aren't scan tokens.
        if name.starts_with("YY") {
            continue;
        }
        highest = highest.max(number);
        values.push(EnumValue {
            rust: variant_name("Token", &name),
            c: name,
            number,
        });
    }
    let position = values
        .iter()
        .position(|v| v.c == "NOT_EQUALS")
        .expect("NOT_EQUALS in gram.h")
        + 1;
    for (offset, name) in ["SQL_COMMENT", "C_COMMENT"].iter().enumerate() {
        values.insert(
            position + offset,
            EnumValue {
                rust: variant_name("Token", name),
                c: (*name).to_string(),
                number: highest + 1 + offset as i64,
            },
        );
    }
    Enum {
        c_name: "Token".into(),
        rust: "Token".into(),
        values,
    }
}

/// The variant name of enumerator `c`: its upper camel case, without the
/// enum's own name when it starts with it (`CmdType`'s `CMD_SELECT` is
/// `CmdSelect`, `Token`'s `ASCII_40` is `Ascii40`).
fn variant_name(enum_rust: &str, c: &str) -> String {
    let name = c.to_upper_camel_case();
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

fn sanitize_ident(ident: &str) -> String {
    match ident {
        "Self" => format!("{ident}_"),
        s if s.starts_with(|c: char| c.is_numeric()) => format!("_{ident}"),
        _ => ident.to_string(),
    }
}

/// The Rust type of a field.
fn rust_type(field: &Field) -> String {
    let inner = match &field.ty {
        FieldType::Bool => "bool".to_string(),
        FieldType::I32 => "i32".to_string(),
        FieldType::I64 => "i64".to_string(),
        FieldType::U32 => "u32".to_string(),
        FieldType::U64 => "u64".to_string(),
        FieldType::F64 => "f64".to_string(),
        FieldType::String => "::std::string::String".to_string(),
        FieldType::Enum(name) => name.clone(),
        FieldType::Node => "Option<Node>".to_string(),
        FieldType::Message(name) => name.clone(),
    };
    if field.repeated {
        assert!(
            !matches!(field.ty, FieldType::Enum(_)),
            "repeated enum field {}",
            field.rust
        );
        format!("Vec<{inner}>")
    } else if matches!(field.ty, FieldType::Message(_)) && !field.embedded {
        format!("Option<NodeBox<{inner}>>")
    } else {
        inner
    }
}

/// A C enum. `Default` is the variant of value 0, what `makeNode`'s zeroing
/// gives, or the first one for an enum without a 0 (`FunctionParameterMode`
/// is a set of characters: `'i'`, `'o'`, …).
fn emit_enum(s: &mut String, enum_def: &Enum) {
    let zero = enum_def
        .values
        .iter()
        .position(|v| v.number == 0)
        .unwrap_or(0);
    let mut seen = HashSet::new();
    for value in &enum_def.values {
        assert!(
            seen.insert(value.number),
            "enum {} repeats the value {}",
            enum_def.rust,
            value.number
        );
    }
    s.push_str(
        "#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, \
         serde::Serialize, serde::Deserialize)]\n",
    );
    s.push_str("#[repr(i32)]\n");
    writeln!(s, "pub enum {} {{", enum_def.rust).unwrap();
    for (index, value) in enum_def.values.iter().enumerate() {
        if index == zero {
            s.push_str("#[default]\n");
        }
        writeln!(s, "{} = {},", value.rust, value.number).unwrap();
    }
    s.push_str("}\n\n");

    writeln!(s, "impl {} {{", enum_def.rust).unwrap();
    writeln!(
        s,
        "/// The enumerator's name in PostgreSQL's sources (`{}`).\n\
         pub fn c_name(self) -> &'static str {{\n\
         match self {{",
        enum_def.values[0].c
    )
    .unwrap();
    for value in &enum_def.values {
        writeln!(s, "Self::{} => {:?},", value.rust, value.c).unwrap();
    }
    s.push_str("}\n}\n}\n\n");
}

#[cfg(test)]
mod tests {
    use heck::ToSnakeCase;

    use super::{field_name, ruby_underscore, variant_name};
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
        assert_eq!(type_ident("A_Const"), "AConst");
        assert_eq!(type_ident("PLAssignStmt"), "PlAssignStmt");
        assert_eq!(type_ident("XmlSerialize"), "XmlSerialize");
        assert_eq!(type_ident("CTECycleClause"), "CteCycleClause");
        assert_eq!(type_ident("SQLValueFunction"), "SqlValueFunction");
    }

    #[test]
    fn field_name_overrides() {
        // `OUTNAME_OVERRIDES[['CreateForeignTableStmt', 'base']]`.
        assert_eq!(field_name("CreateForeignTableStmt", "base"), "base_stmt");
        assert_eq!(
            field_ident(&field_name("CreateForeignTableStmt", "base")),
            "base_stmt"
        );
        // Other nodes' `base` keeps its name.
        assert_eq!(field_name("OtherStmt", "base"), "base");
        // `queryId` is a plain camelCase field elsewhere.
        assert_eq!(field_name("OtherStmt", "queryId"), "query_id");
        // `ruby_underscore` and `heck` agree on a plain snake_case name.
        assert_eq!("base_stmt".to_snake_case(), "base_stmt");
    }

    #[test]
    fn variants_drop_the_enum_name() {
        assert_eq!(variant_name("CmdType", "CMD_SELECT"), "CmdSelect");
        assert_eq!(variant_name("SetOperation", "SETOP_UNION"), "SetopUnion");
        assert_eq!(variant_name("Token", "ASCII_40"), "Ascii40");
        assert_eq!(variant_name("Token", "IDENT"), "Ident");
    }
}
