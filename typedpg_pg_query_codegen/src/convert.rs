//! Generation of the C parse tree → AST converter.
//!
//! The next phase switches `typedpg_pg_query` from libpg_query to PostgreSQL's
//! own parser; [`crate::generate_from_headers`] emits this module alongside the
//! AST so that the raw C tree `raw_parser()` returns can be turned into the
//! `protobuf` records (the name is historical — they are plain Rust structs).
//!
//! The conversion is a Rust port of libpg_query's
//! `src/pg_query_outfuncs_protobuf.c` plus the generated
//! `src/include/pg_query_outfuncs_defs.c` and `pg_query_outfuncs_conds.c`: the
//! same fields are copied, in the same shapes, because the next phase compares
//! the trees against libpg_query's over tens of thousands of statements.
//!
//! What the C backend's `WRITE_*` macros do, and what this generates:
//!
//! - a NULL `char *` becomes the empty string; a `char` becomes a one-character
//!   string, or empty for `'\0'`;
//! - an enum becomes its `_enumToInt` value: its position in the declaration
//!   (one-based; the `.proto` has a dummy zero value before the first
//!   enumerator), which differs from the C value for enums with negative or
//!   non-sequential values;
//! - `List *` becomes a repeated `Node`, each cell through [`node`]; a
//!   `Bitmapset *` becomes a repeated `uint64` of its members, ascending;
//! - a typed node pointer (`RangeVar *`, `Alias *`, …) becomes its own message
//!   (not wrapped in `Node`), and only an abstract `Node *` / `Expr *` /
//!   `Value *` becomes a `Node`;
//! - an embedded node struct (only `CreateForeignTableStmt.base`) becomes its
//!   message;
//! - `A_Const` is special: `isnull`, `location`, then the `val` oneof chosen by
//!   the tag of the union member.
//!
//! Two differences from the C backend are deliberate:
//!
//! - an unknown `NodeTag` makes [`node`] return `None` where the C backend
//!   logs a warning and leaves the field unset (the next phase turns the `None`
//!   into an error);
//! - the C backend's `WRITE_LIST_FIELD` reads every list cell as a pointer,
//!   which cannot work for an `IntList`/`OidList`; here those cells become
//!   `Integer` nodes (the only integer node the AST has), and no raw parse
//!   tree produced by `raw_parser()` contains one.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;

use heck::ToSnakeCase;

use crate::ast::{Program, node_struct};
use crate::schema::Schema;
use crate::{Field, FieldType, HEADER_FROM_HEADERS, Message};

/// Generate `src/convert.rs`.
pub fn generate(schema: &Schema, program: &Program) -> String {
    let mut s = String::from(HEADER_FROM_HEADERS);
    s.push_str(PRELUDE);

    let c_names = c_struct_names(schema);
    let enum_values = enum_values(schema);
    let mut used_enums = BTreeSet::new();
    let node_message = program
        .model
        .messages
        .iter()
        .find(|m| m.name == "Node")
        .expect("the Node message");
    let boxed = |ty: &str| -> bool {
        node_message
            .fields
            .iter()
            .find(|f| matches!(&f.ty, FieldType::Message(t) if t == ty))
            .map(|f| f.boxed)
            .unwrap_or(false)
    };

    emit_node(&mut s, program, schema, &boxed);
    emit_raw_stmts(&mut s);

    for message in &program.model.messages {
        match message.name.as_str() {
            "Node" | "ParseResult" | "ScanResult" | "ScanToken" | "SummaryResult" => continue,
            "Integer" | "Float" | "Boolean" | "String" | "BitString" | "List" | "IntList"
            | "OidList" | "A_Const" => emit_value(&mut s, message),
            _ => {
                let Some((struct_def, _)) = node_struct(schema, &message.name) else {
                    continue;
                };
                emit_struct(
                    &mut s,
                    message,
                    &struct_def,
                    &c_names,
                    &enum_values,
                    &mut used_enums,
                );
            }
        }
    }

    for enum_name in used_enums {
        emit_enum(&mut s, &enum_name, &enum_values[&enum_name]);
    }

    s
}

/// The helpers every generated function uses. Kept here rather than generated
/// per call so the per-struct bodies stay close to the C macros.
const PRELUDE: &str = r#"//! Conversion of PostgreSQL's C raw parse tree into the AST types.
//!
//! Generated together with `protobuf.rs`; it assumes `mod bindings` and
//! `mod protobuf` are siblings, as the crate arranges them.

#![allow(clippy::all, dead_code, unused_variables)]

use crate::bindings;
use crate::protobuf;

/// The deepest message nesting the AST can carry, libpg_query's
/// `PG_QUERY_PROTOBUF_MAX_DEPTH`: deeper trees could not be encoded to
/// protobuf (nor decoded back) by the engine this replaces, so they are
/// refused here too.
const MAX_DEPTH: usize = 10_000;

thread_local! {
    /// The message nesting of the conversion in progress. Every `Node`
    /// wrapper and every message counts one level, as a protobuf encoder
    /// counts them.
    static DEPTH: ::core::cell::Cell<usize> = const { ::core::cell::Cell::new(0) };
    /// Whether [`MAX_DEPTH`] was passed; the conversion then finishes with
    /// default values and the caller turns this into an error.
    static TOO_DEEP: ::core::cell::Cell<bool> = const { ::core::cell::Cell::new(false) };
}

/// Counts one message level for its lifetime.
struct DepthGuard(());

impl DepthGuard {
    fn new() -> DepthGuard {
        DEPTH.with(|depth| {
            let now = depth.get() + 1;
            depth.set(now);
            if now > MAX_DEPTH {
                TOO_DEEP.with(|flag| flag.set(true));
            }
        });
        DepthGuard(())
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Start a conversion: clears the depth state of a previous one.
pub(crate) fn begin() {
    DEPTH.with(|depth| depth.set(0));
    TOO_DEEP.with(|flag| flag.set(false));
}

/// Whether the conversion passed [`MAX_DEPTH`].
pub(crate) fn too_deep() -> bool {
    TOO_DEEP.with(|flag| flag.get())
}

/// Run one message's conversion on enough stack: the recursion is as deep as
/// the tree (up to [`MAX_DEPTH`] levels), so the stack grows in segments as it
/// goes instead of being reserved up front.
#[inline(always)]
fn grow<R>(f: impl FnOnce() -> R) -> R {
    ::stacker::maybe_grow(64 * 1024, 1024 * 1024, f)
}

/// A C string as a Rust string; NULL is the empty string (`WRITE_STRING_FIELD`
/// leaves the default in place).
///
/// # Safety
/// `p` must be NULL or point to a NUL-terminated string.
unsafe fn c_string(p: *const ::core::ffi::c_char) -> ::std::string::String {
    if p.is_null() {
        ::std::string::String::new()
    } else {
        unsafe { ::std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned()
    }
}

/// One `char` as a one-character string; `'\0'` is empty
/// (`WRITE_CHAR_FIELD`).
fn c_char_string(c: ::core::ffi::c_char) -> ::std::string::String {
    if c == 0 {
        ::std::string::String::new()
    } else {
        ::std::string::String::from_utf8_lossy(&[c as u8]).into_owned()
    }
}

/// The members of a bitmapset, ascending, as `WRITE_BITMAPSET_FIELD` reads
/// them with `bms_next_member`; NULL and the empty set are empty.
///
/// # Safety
/// `p` must be NULL or point to a valid `Bitmapset`.
unsafe fn bitmapset_members(p: *mut bindings::Bitmapset) -> ::std::vec::Vec<u64> {
    let mut out = ::std::vec::Vec::new();
    if p.is_null() {
        return out;
    }
    let p = unsafe { &*p };
    let words = unsafe { p.words.as_slice(p.nwords.max(0) as usize) };
    let bits_per_word = ::core::mem::size_of::<bindings::bitmapword>() * 8;
    for (index, word) in words.iter().enumerate() {
        let mut word = *word;
        while word != 0 {
            let bit = word.trailing_zeros() as usize;
            out.push((index * bits_per_word + bit) as u64);
            word &= word - 1;
        }
    }
    out
}

/// The cells of a `List *` as `Node`s, as `WRITE_LIST_FIELD` /
/// `_outList` do. NULL and the empty list are empty.
///
/// A `T_IntList`/`T_OidList` cell holds an integer, not a pointer; it becomes
/// an `Integer` node, which is how the AST can represent it.
///
/// # Safety
/// `list` must be NULL or point to a valid `List`.
unsafe fn list_items(list: *mut bindings::List) -> ::std::vec::Vec<protobuf::Node> {
    let mut out = ::std::vec::Vec::new();
    if list.is_null() {
        return out;
    }
    let list = unsafe { &*list };
    let length = list.length.max(0) as usize;
    out.reserve(length);
    for index in 0..length {
        let cell = unsafe { &*list.elements.add(index) };
        let node = match list.type_ {
            bindings::NodeTag_T_IntList => Some(protobuf::node::Node::Integer(protobuf::Integer {
                ival: unsafe { cell.int_value },
            })),
            bindings::NodeTag_T_OidList => Some(protobuf::node::Node::Integer(protobuf::Integer {
                ival: unsafe { cell.oid_value } as i32,
            })),
            _ => unsafe { node(cell.ptr_value as *const bindings::Node) },
        };
        // `_outNode` still appends the (empty) `Node` message for a NULL or
        // unrecognized cell; only the oneof stays unset.
        out.push(protobuf::Node { node });
    }
    out
}

"#;

/// `NodeTag` value → the C type of the node and the function that converts it.
fn c_struct_names(schema: &Schema) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for nodetype in &schema.nodetypes {
        out.insert(normalised(nodetype), nodetype.clone());
    }
    out
}

/// The C enums and their enumerators, by C type name.
fn enum_values(schema: &Schema) -> HashMap<String, Vec<(String, i64)>> {
    let mut out = HashMap::new();
    for group in &schema.groups {
        for enum_def in &group.enums {
            if let Some(name) = &enum_def.name {
                out.insert(name.clone(), enum_def.resolved_values());
            }
        }
    }
    out
}

/// `_enumToInt` as libpg_query generates it: the enumerators number from one
/// in declaration order, which is not the C value when the enum is not
/// sequential from zero (`PARTITION_RANGE_DATUM_MINVALUE` is -1) or has gaps.
fn emit_enum(s: &mut String, c_name: &str, values: &[(String, i64)]) {
    let name = format!("convert_{}", c_name.to_snake_case());
    let _ = writeln!(
        s,
        "fn {name}(value: bindings::{c_name}) -> i32 {{\n    match value {{"
    );
    for (index, (variant, _)) in values.iter().enumerate() {
        let _ = writeln!(s, "        bindings::{c_name}_{variant} => {},", index + 1);
    }
    s.push_str("        _ => -1,\n    }\n}\n\n");
}

/// The lookups below go from the Rust message name to the C type name; both
/// normalise the same way (`A_Const`/`AConst` → `aconst`,
/// `SQLValueFunction`/`SqlValueFunction` → `sqlvaluefunction`).
fn normalised(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn c_type_name(c_names: &HashMap<String, String>, rust: &str) -> String {
    c_names
        .get(&normalised(rust))
        .cloned()
        .unwrap_or_else(|| rust.to_string())
}

fn emit_node(s: &mut String, program: &Program, schema: &Schema, boxed: &impl Fn(&str) -> bool) {
    s.push_str(
        "/// Convert one C node, by its `NodeTag`; NULL is `None`.\n\
         ///\n\
         /// An unrecognized tag also returns `None` (libpg_query's C backend\n\
         /// logs a warning and leaves the field unset; the caller turns this\n\
         /// into an error).\n\
         ///\n\
         /// # Safety\n\
         /// `p` must be NULL or point to a live node.\n\
         pub(crate) unsafe fn node(p: *const bindings::Node) -> Option<protobuf::node::Node> {\n\
         \x20   if too_deep() {\n\
         \x20       return None;\n\
         \x20   }\n\
         \x20   let _depth = DepthGuard::new();\n\
         \x20   if p.is_null() {\n\
         \x20       return None;\n\
         \x20   }\n\
         \x20   let tag = unsafe { (*p).type_ };\n\
         \x20   Some(match tag {\n",
    );
    for (variant, ty) in &program.model.variants {
        let tag = variant_c_name(schema, ty);
        let binding = binding_type(&tag);
        let call = format!(
            "unsafe {{ {}(p as *const bindings::{binding}) }}",
            ty.to_snake_case()
        );
        let call = if boxed(ty) {
            format!("::std::boxed::Box::new({call})")
        } else {
            call
        };
        writeln!(
            s,
            "        bindings::NodeTag_T_{tag} => protobuf::node::Node::{variant}({call}),"
        )
        .unwrap();
    }
    s.push_str("        _ => return None,\n    })\n}\n\n");
}

/// The C type bindgen knows a node tag by. The `IntList` and `OidList` tags
/// have no struct of their own — they are `List`s with another tag — and
/// `A_Const` is spelled `AConst` in the AST.
fn binding_type(tag: &str) -> &str {
    match tag {
        "IntList" | "OidList" => "List",
        other => other,
    }
}

/// The C name behind a Rust message name (`AConst` → `A_Const`, `CteCycleClause`
/// → `CTECycleClause`).
fn variant_c_name(schema: &Schema, rust: &str) -> String {
    for nodetype in &schema.nodetypes {
        if normalised(nodetype) == normalised(rust) {
            return nodetype.clone();
        }
    }
    rust.to_string()
}

fn emit_raw_stmts(s: &mut String) {
    s.push_str(
        "/// Convert a raw parse tree (a `List` of `RawStmt`) into the AST's\n\
         /// statements; NULL is empty.\n\
         ///\n\
         /// # Safety\n\
         /// `list` must be NULL or point to a valid `List` of `RawStmt`.\n\
         pub(crate) unsafe fn raw_stmts(list: *const bindings::List) -> ::std::vec::Vec<protobuf::RawStmt> {\n\
         \x20   begin();\n\
         \x20   let mut out = ::std::vec::Vec::new();\n\
         \x20   if list.is_null() {\n\
         \x20       return out;\n\
         \x20   }\n\
         \x20   let list = unsafe { &*list };\n\
         \x20   let length = list.length.max(0) as usize;\n\
         \x20   out.reserve(length);\n\
         \x20   for index in 0..length {\n\
         \x20       let cell = unsafe { &*list.elements.add(index) };\n\
         \x20       out.push(unsafe { raw_stmt(cell.ptr_value as *const bindings::RawStmt) });\n\
         \x20   }\n\
         \x20   out\n}\n\n",
    );
}

/// The value and list nodes, which `nodes/value.h` and `nodes/pg_list.h`
/// declare but whose AST shapes are the generator's own (see
/// [`crate::ast::value_messages`]).
fn emit_value(s: &mut String, message: &Message) {
    let name = message.rust.to_snake_case();
    match message.name.as_str() {
        "Integer" => {
            let _ = writeln!(
                s,
                "pub(crate) unsafe fn {name}(p: *const bindings::Integer) -> protobuf::{ty} {{\n\
                 \x20   if too_deep() {{\n\
         \x20       return protobuf::{ty}::default();\n\
         \x20   }}\n\
         \x20   let _depth = DepthGuard::new();\n\
                 \x20   let mut out = protobuf::{ty}::default();\n\
                 \x20   out.ival = unsafe {{ (*p).ival as i32 }};\n\
                 \x20   out\n}}\n",
                ty = message.rust,
            );
        }
        "Float" => value_string(s, message, "Float", "fval"),
        "Boolean" => {
            let _ = writeln!(
                s,
                "pub(crate) unsafe fn {name}(p: *const bindings::Boolean) -> protobuf::{ty} {{\n\
                 \x20   if too_deep() {{\n\
         \x20       return protobuf::{ty}::default();\n\
         \x20   }}\n\
         \x20   let _depth = DepthGuard::new();\n\
                 \x20   let mut out = protobuf::{ty}::default();\n\
                 \x20   out.boolval = unsafe {{ (*p).boolval }};\n\
                 \x20   out\n}}\n",
                ty = message.rust,
            );
        }
        "String" => value_string(s, message, "String", "sval"),
        "BitString" => value_string(s, message, "BitString", "bsval"),
        "List" | "IntList" | "OidList" => {
            let _ = writeln!(
                s,
                "pub(crate) unsafe fn {name}(p: *const bindings::List) -> protobuf::{ty} {{\n\
                 \x20   if too_deep() {{\n\
         \x20       return protobuf::{ty}::default();\n\
         \x20   }}\n\
         \x20   let _depth = DepthGuard::new();\n\
                 \x20   let mut out = protobuf::{ty}::default();\n\
                 \x20   out.items = unsafe {{ list_items(p as *mut bindings::List) }};\n\
                 \x20   out\n}}\n",
                ty = message.rust,
            );
        }
        "A_Const" => emit_a_const(s, message),
        other => panic!("unexpected value node {other}"),
    }
}

fn value_string(s: &mut String, message: &Message, c: &str, field: &str) {
    let name = message.rust.to_snake_case();
    let _ = writeln!(
        s,
        "pub(crate) unsafe fn {name}(p: *const bindings::{c}) -> protobuf::{ty} {{\n\
         \x20   if too_deep() {{\n\
         \x20       return protobuf::{ty}::default();\n\
         \x20   }}\n\
         \x20   let _depth = DepthGuard::new();\n\
         \x20   let mut out = protobuf::{ty}::default();\n\
         \x20   out.{field} = unsafe {{ c_string((*p).{field}) }};\n\
         \x20   out\n}}\n",
        ty = message.rust,
    );
}

fn emit_a_const(s: &mut String, message: &Message) {
    let name = message.rust.to_snake_case();
    let ty = &message.rust;
    let members = [
        ("Integer", "Ival", "ival", "ival"),
        ("Float", "Fval", "fval", "fval"),
        ("Boolean", "Boolval", "boolval", "boolval"),
        ("String", "Sval", "sval", "sval"),
        ("BitString", "Bsval", "bsval", "bsval"),
    ];
    let _ = writeln!(
        s,
        "pub(crate) unsafe fn {name}(p: *const bindings::A_Const) -> protobuf::{ty} {{\n\
         \x20   if too_deep() {{\n\
         \x20       return protobuf::{ty}::default();\n\
         \x20   }}\n\
         \x20   let _depth = DepthGuard::new();\n\
         \x20   let mut out = protobuf::{ty}::default();\n\
         \x20   out.isnull = unsafe {{ (*p).isnull }};\n\
         \x20   out.location = unsafe {{ (*p).location as i32 }};\n\
         \x20   if !out.isnull {{\n\
         \x20       out.val = unsafe {{\n\
         \x20           match (*p).val.node.type_ {{\n"
    );
    for (c_type, variant, union_field, ast_field) in members {
        let value = match c_type {
            "Integer" => {
                format!("protobuf::Integer {{ ival: (*p).val.{union_field}.{ast_field} as i32 }}")
            }
            "Boolean" => {
                format!("protobuf::Boolean {{ boolval: (*p).val.{union_field}.{ast_field} }}")
            }
            _ => format!(
                "protobuf::{c_type} {{ {ast_field}: c_string((*p).val.{union_field}.{ast_field}) }}"
            ),
        };
        let _ = writeln!(
            s,
            "                bindings::NodeTag_T_{c_type} => Some(protobuf::a_const::Val::{variant}({value})),"
        );
    }
    s.push_str(
        "                _ => None,\n\
         \x20           }\n\
         \x20       };\n\
         \x20   }\n\
         \x20   out\n}\n\n",
    );
    let _ = ty;
}

fn emit_struct(
    s: &mut String,
    message: &Message,
    struct_def: &crate::schema::StructDef,
    c_names: &HashMap<String, String>,
    enum_values: &HashMap<String, Vec<(String, i64)>>,
    used_enums: &mut BTreeSet<String>,
) {
    let name = message.rust.to_snake_case();
    let ty = &message.rust;
    let _ = writeln!(
        s,
        "pub(crate) unsafe fn {name}(p: *const bindings::{c_ty}) -> protobuf::{ty} {{",
        c_ty = message.name,
    );

    let mut body = String::new();
    for field in &message.fields {
        if field.oneof.is_some() {
            continue;
        }
        let Some(cdef) = struct_def
            .fields
            .iter()
            .find(|f| f.name.as_deref() == Some(field.json.as_str()))
        else {
            panic!(
                "{}.{} is in the AST but not in the C struct",
                message.name, field.json
            );
        };
        let c_type = cdef
            .c_type
            .as_deref()
            .unwrap_or_else(|| panic!("{}.{} has no C type", message.name, field.json));
        // The abstract `Expr xpr` every expression node embeds is not part of
        // the generated `_out*` functions (only its `NodeTag` exists); the AST
        // field stays unset.
        if !c_type.ends_with('*') && matches!(&field.ty, FieldType::Message(ty) if ty == "Node") {
            continue;
        }
        let expr = conversion_expr(field, c_type, c_names, enum_values, used_enums);
        let _ = writeln!(body, "    out.{} = unsafe {{ {expr} }};", field.rust);
    }

    let binding = if body.is_empty() { "let" } else { "let mut" };
    let _ = writeln!(
        s,
        "    if too_deep() {{\n        return protobuf::{ty}::default();\n    }}\n    let _depth = DepthGuard::new();\n    grow(|| {{\n    {binding} out = protobuf::{ty}::default();\n{body}    out\n    }})\n}}\n"
    );
}

/// The Rust expression (evaluated with `p` in scope) that converts one C field
/// into its AST field.
fn conversion_expr(
    field: &Field,
    c_type: &str,
    c_names: &HashMap<String, String>,
    enum_values: &HashMap<String, Vec<(String, i64)>>,
    used_enums: &mut BTreeSet<String>,
) -> String {
    // `WRITE_LIST_FIELD`: a `List *` field is a repeated `Node`.
    if field.repeated && matches!(&field.ty, FieldType::Message(ty) if ty == "Node") {
        return format!("list_items((*p).{})", c_field_ident(&field.json));
    }
    if matches!(c_type, "Bitmapset*" | "Relids") {
        return format!("bitmapset_members((*p).{})", c_field_ident(&field.json));
    }

    let value = format!("(*p).{}", c_field_ident(&field.json));
    match &field.ty {
        FieldType::Bool => value,
        FieldType::I32 => format!("{value} as i32"),
        FieldType::I64 => format!("{value} as i64"),
        FieldType::U32 => format!("{value} as u32"),
        FieldType::U64 => format!("{value} as u64"),
        FieldType::F64 => format!("{value} as f64"),
        FieldType::String => {
            if c_type == "char" {
                format!("c_char_string({value})")
            } else {
                format!("c_string({value})")
            }
        }
        // `WRITE_ENUM_FIELD`: `_enumToInt` numbers the enumerators from one.
        FieldType::Enum(_) => {
            if enum_values.contains_key(c_type) {
                used_enums.insert(c_type.to_string());
                format!("convert_{}({value})", c_type.to_snake_case())
            } else {
                format!("({value} as i32) + 1")
            }
        }
        FieldType::Message(ty) if ty == "Node" => {
            if !c_type.ends_with('*') {
                panic!(
                    "{}.{} embeds an abstract node; only `Expr xpr` does, and it is skipped",
                    field.json, field.json
                );
            }
            let call = format!(
                "if {value}.is_null() {{ None }} else {{ Some(protobuf::Node {{ node: node({value} as *const bindings::Node) }}) }}"
            );
            if field.boxed {
                format!(
                    "if {value}.is_null() {{ None }} else {{ Some(::std::boxed::Box::new(protobuf::Node {{ node: node({value} as *const bindings::Node) }})) }}"
                )
            } else {
                call
            }
        }
        FieldType::Message(ty) => {
            let c_ty = c_type_name(c_names, ty);
            let call = if c_type.ends_with('*') {
                format!(
                    "if {value}.is_null() {{ None }} else {{ Some({}({value} as *const bindings::{c_ty})) }}",
                    ty.to_snake_case()
                )
            } else {
                format!(
                    "Some({}(&{value} as *const bindings::{c_ty}))",
                    ty.to_snake_case()
                )
            };
            if field.boxed {
                if c_type.ends_with('*') {
                    format!(
                        "if {value}.is_null() {{ None }} else {{ Some(::std::boxed::Box::new({}({value} as *const bindings::{c_ty}))) }}",
                        ty.to_snake_case()
                    )
                } else {
                    format!(
                        "Some(::std::boxed::Box::new({}(&{value} as *const bindings::{c_ty})))",
                        ty.to_snake_case()
                    )
                }
            } else {
                call
            }
        }
        FieldType::Map => panic!("map field {} in the C tree", field.json),
    }
}

/// The Rust identifier bindgen gives a C field: a keyword gets a trailing
/// underscore (`type` → `type_`, `override` → `override_`).
fn c_field_ident(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "override",
        "priv", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true",
        "type", "unsafe", "use", "where", "while", "async", "await", "dyn", "abstract", "become",
        "box", "do", "final", "macro", "typeof", "unsized", "virtual", "yield", "try",
    ];
    if KEYWORDS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::c_field_ident;

    #[test]
    fn c_fields_become_bindgen_identifiers() {
        assert_eq!(c_field_ident("aliasname"), "aliasname");
        assert_eq!(c_field_ident("type"), "type_");
        assert_eq!(c_field_ident("override"), "override_");
    }
}
