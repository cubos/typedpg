//! PostgreSQL's node headers, read the way libpg_query's
//! `scripts/extract_headers.rb` reads them.
//!
//! libpg_query turns PostgreSQL's headers into AST definitions with a
//! line-oriented Ruby script: [`extract_headers.rb`] produces
//! `srcdata/*.json`, and `generate_protobuf_and_funcs.rb` turns that into
//! `pg_query.proto`. This module is the first half of dropping the `.proto`
//! step: it is a port of `extract_headers.rb`, returning the same data as a
//! typed [`Schema`].
//!
//! Every parser function names the Ruby rule it mirrors. The Ruby script is
//! deliberately forgiving — it matches field declarations with one regular
//! expression per line rather than parsing C — and the port keeps that
//! behaviour, because the generated AST has to stay identical to what
//! libpg_query's `.proto` describes (including the fields the script
//! accidentally drops, such as those carrying more than one
//! `pg_node_attr(...)` argument).
//!
//! [`extract_headers.rb`]: https://github.com/pganalyze/libpg_query/blob/master/scripts/extract_headers.rb

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// The header groups `generate_defs!` walks, in order. `../backend/parser/gram`
/// is PostgreSQL's generated grammar header (passed separately, since a build
/// tree keeps it outside `src/include`); `../backend/parser/gramparse` is only
/// read for completeness — none of its definitions reach the AST.
const GROUPS: &[&str] = &[
    "nodes/parsenodes",
    "nodes/primnodes",
    "nodes/lockoptions",
    "nodes/nodes",
    "nodes/params",
    "access/attnum",
    "access/cmptype",
    "c",
    "postgres",
    "postgres_ext",
    "commands/vacuum",
    "storage/block",
    "access/sdir",
    "mb/pg_wchar",
    "../backend/parser/gram",
    "../backend/parser/gramparse",
];

/// The `Node` types `extract_headers.rb` skips by name.
const IGNORE_LIST: &[&str] = &[
    "Node",
    "varlena",
    "IntArray",
    "nameData",
    "bool",
    "sig_atomic_t",
    "size_t",
    "varatt_indirect",
    "A_Const",
];

/// The headers, parsed.
#[derive(Debug, Default)]
pub struct Schema {
    /// Node tags, in the order `nodes/nodetags.h` declares them. Each entry is
    /// the type name without the leading `T_`.
    pub nodetypes: Vec<String>,
    /// Groups in [`GROUPS`] order.
    pub groups: Vec<Group>,
    /// `typedef old new;` declarations, in header order.
    pub typedefs: Vec<Typedef>,
}

/// One header group (`nodes/parsenodes.h`, …).
#[derive(Debug)]
pub struct Group {
    /// The group's name, as in [`GROUPS`] (`"nodes/parsenodes"`).
    pub name: String,
    pub structs: Vec<StructDef>,
    pub enums: Vec<EnumDef>,
}

/// A `typedef struct` definition (mirrors one entry of
/// `struct_defs.json`).
#[derive(Debug, Clone)]
pub struct StructDef {
    /// The `struct` tag (`"A_Expr"`).
    pub name: String,
    pub fields: Vec<FieldDef>,
}

/// One field line. The Ruby script also pushes comment-only entries (plain
/// `{comment: …}` objects) for blank lines and comments; they have no name or
/// type and are kept here so the line-driven state machine ports unchanged.
#[derive(Debug, Clone)]
pub struct FieldDef {
    /// PostgreSQL's field name (`"lexpr"`), when the line declares a field.
    pub name: Option<String>,
    /// The C type as the Ruby script assembles it from the type word and the
    /// pointer stars (`"Node*"`, `"List*"`, …).
    pub c_type: Option<String>,
}

/// A `typedef enum` definition (mirrors one entry of `enum_defs.json`).
#[derive(Debug, Clone)]
pub struct EnumDef {
    /// The enum tag; anonymous enums take the name after the closing brace.
    pub name: Option<String>,
    pub values: Vec<EnumValue>,
}

/// One enumerator. Like the Ruby script, `value` is `None` when the source
/// line only continues an implicitly numbered sequence; resolution happens in
/// [`EnumDef::resolved_values`].
#[derive(Debug, Clone)]
pub struct EnumValue {
    pub name: Option<String>,
    pub value: Option<i64>,
}

/// A `typedef` alias.
#[derive(Debug, Clone)]
pub struct Typedef {
    /// The new type name (`"DistinctExpr"`).
    pub name: String,
    /// The aliased type (`"OpExpr"`); may contain spaces (`"unsigned int"`).
    pub source: String,
}

impl EnumDef {
    /// The enumerators with their values, computing the implicit ones exactly
    /// as `Extractor#handle_enum` does: an initializer is either decimal,
    /// shifted (`1 << 3`), `PG_INT32_MAX`, a character literal, or one more
    /// than the previous value (zero if there is none).
    pub fn resolved_values(&self) -> Vec<(String, i64)> {
        let mut out = Vec::new();
        let mut last: Option<i64> = None;
        for value in &self.values {
            let Some(name) = &value.name else { continue };
            let number = value.value.or_else(|| last.map(|v| v + 1)).unwrap_or(0);
            last = Some(number);
            out.push((name.clone(), number));
        }
        out
    }
}

impl Schema {
    /// Parse PostgreSQL's headers: `include_dir` is the directory configure's
    /// headers live in (PostgreSQL's `src/include`, after `make
    /// generated-headers`), `gram_h` the generated `gram.h` from the build
    /// tree. Mirrors `Extractor#extract!`.
    pub fn parse(include_dir: &Path, gram_h: &Path) -> Schema {
        let mut schema = Schema {
            nodetypes: parse_nodetags(&include_dir.join("nodes/nodetags.h")),
            ..Schema::default()
        };

        for group in GROUPS {
            let path = match *group {
                "../backend/parser/gram" => gram_h.to_path_buf(),
                "../backend/parser/gramparse" => include_dir
                    .parent()
                    .unwrap_or(include_dir)
                    .join("backend/parser/gramparse.h"),
                _ => include_dir.join(format!("{group}.h")),
            };
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                // Only gramparse.h is allowed to be absent: a PostgreSQL build
                // tree keeps it next to gram.h, and none of its definitions are
                // AST types.
                Err(_) if *group == "../backend/parser/gramparse" => continue,
                Err(e) => panic!("reading {}: {e}", path.display()),
            };
            let (structs, enums, typedefs) = parse_group(&text);
            schema.typedefs.extend(typedefs);
            schema.groups.push(Group {
                name: (*group).to_string(),
                structs,
                enums,
            });
        }

        schema.fixup();
        schema
    }

    /// The fixups at the end of `Extractor#extract!`: node tags become
    /// `NodeTag` values, and the value/list nodes are synthesized (the Ruby
    /// script defines them by hand rather than parsing them).
    fn fixup(&mut self) {
        let nodetypes = self.nodetypes.clone();
        let node_tag = self
            .group_mut("nodes/nodes")
            .and_then(|g| {
                g.enums
                    .iter_mut()
                    .find(|e| e.name.as_deref() == Some("NodeTag"))
            })
            .expect("NodeTag in nodes/nodes.h");
        for (i, name) in nodetypes.iter().enumerate() {
            node_tag.values.push(EnumValue {
                name: Some(format!("T_{name}")),
                value: Some(i as i64 + 1),
            });
        }

        let value = [
            ("Integer", "ival", "long"),
            ("Float", "fval", "char*"),
            ("Boolean", "boolval", "bool"),
            ("String", "sval", "char*"),
            ("BitString", "bsval", "char*"),
        ];
        let mut structs = Vec::new();
        for (name, field, c_type) in value {
            structs.push(StructDef {
                name: name.to_string(),
                fields: vec![FieldDef {
                    name: Some(field.to_string()),
                    c_type: Some(c_type.to_string()),
                }],
            });
        }
        structs.push(StructDef {
            name: "A_Const".to_string(),
            fields: vec![
                FieldDef {
                    name: Some("isnull".to_string()),
                    c_type: Some("bool".to_string()),
                },
                FieldDef {
                    name: Some("val".to_string()),
                    c_type: Some("Node".to_string()),
                },
            ],
        });
        self.groups.push(Group {
            name: "nodes/value".to_string(),
            structs,
            enums: Vec::new(),
        });

        self.groups.push(Group {
            name: "nodes/pg_list".to_string(),
            structs: vec![StructDef {
                name: "List".to_string(),
                fields: vec![FieldDef {
                    name: Some("items".to_string()),
                    c_type: Some("[]Node".to_string()),
                }],
            }],
            enums: Vec::new(),
        });

        // `@struct_defs['nodes/params']['ParamListInfoData'][:fields].reject!`
        if let Some(params) = self.group_mut("nodes/params")
            && let Some(struct_def) = params
                .structs
                .iter_mut()
                .find(|s| s.name == "ParamListInfoData")
        {
            struct_def
                .fields
                .retain(|f| f.c_type.as_deref() != Some("ParamExternData"));
        }
    }

    pub fn group(&self, name: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.name == name)
    }

    fn group_mut(&mut self, name: &str) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.name == name)
    }

    /// A struct's definition in `group`.
    pub fn struct_def(&self, group: &str, name: &str) -> Option<&StructDef> {
        self.group(group)?.structs.iter().find(|s| s.name == name)
    }

    /// An enum's definition in `group`.
    pub fn enum_def(&self, group: &str, name: &str) -> Option<&EnumDef> {
        self.group(group)?
            .enums
            .iter()
            .find(|e| e.name.as_deref() == Some(name))
    }

    /// The enum definitions of `group`, in header order.
    pub fn enums(&self, group: &str) -> &[EnumDef] {
        self.group(group).map(|g| g.enums.as_slice()).unwrap_or(&[])
    }

    /// The typedef whose new name is `name`.
    pub fn typedef(&self, name: &str) -> Option<&Typedef> {
        self.typedefs.iter().find(|t| t.name == name)
    }

    pub fn is_nodetype(&self, name: &str) -> bool {
        self.nodetypes.iter().any(|n| n == name)
    }
}

/// `Extractor#generate_nodetypes!`: lines of `nodes/nodetags.h` that are not
/// comment continuations and hold a `T_…` tag.
fn parse_nodetags(path: &Path) -> Vec<String> {
    static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/? *\*").unwrap());
    static TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"T_([A-z_]+)(\s+=\s+\d+)?,").unwrap());

    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut out = Vec::new();
    for line in text.lines() {
        if COMMENT.is_match(line) {
            continue;
        }
        if let Some(captures) = TAG.captures(line)
            && let Some(name) = captures.get(1)
        {
            out.push(name.as_str().to_string());
        }
    }
    out
}

/// `Extractor#generate_defs!` for one header: the line-driven state machine
/// that collects structs, enums and typedefs.
fn parse_group(text: &str) -> (Vec<StructDef>, Vec<EnumDef>, Vec<Typedef>) {
    static STRUCT_START: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:typedef )?struct ([A-z]+)\s*(/\*.+)?$").unwrap());
    static ENUM_START: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*(?:typedef )?enum\s*([A-z]+)?\s*(/\*.+)?(?: \{)?$").unwrap()
    });
    static TYPEDEF: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^typedef( struct)? ([A-z0-9\s_]+) \*?([A-z]+);").unwrap());

    let mut structs = Vec::new();
    let mut enums = Vec::new();
    let mut typedefs = Vec::new();
    let mut current_struct: Option<StructDef> = None;
    let mut current_enum: Option<EnumDef> = None;
    let mut open_comment = false;

    for line in text.lines() {
        if current_struct.is_some() {
            handle_struct(line, &mut current_struct, &mut structs, &mut open_comment);
        } else if current_enum.is_some() {
            handle_enum(line, &mut current_enum, &mut enums, &mut open_comment);
        } else if let Some(captures) = STRUCT_START.captures(line) {
            let name = captures.get(1).expect("struct name").as_str();
            if IGNORE_LIST.contains(&name) {
                continue;
            }
            current_struct = Some(StructDef {
                name: name.to_string(),
                fields: Vec::new(),
            });
            open_comment = false;
        } else if let Some(captures) = ENUM_START.captures(line) {
            let name = captures.get(1).map(|m| m.as_str().to_string());
            if name.as_deref().is_some_and(|n| IGNORE_LIST.contains(&n)) {
                continue;
            }
            current_enum = Some(EnumDef {
                name,
                values: Vec::new(),
            });
            open_comment = false;
        } else if let Some(captures) = TYPEDEF.captures(line) {
            let source = captures.get(2).expect("source type").as_str();
            let name = captures.get(3).expect("typedef name").as_str();
            if IGNORE_LIST.contains(&source) || IGNORE_LIST.contains(&name) {
                continue;
            }
            typedefs.push(Typedef {
                name: name.to_string(),
                source: source.to_string(),
            });
            open_comment = false;
        } else if line.trim_start().starts_with("/*") || open_comment {
            open_comment = !line.contains("*/");
        }
    }

    assert!(
        current_struct.is_none() && current_enum.is_none(),
        "the header ends inside a definition"
    );
    (structs, enums, typedefs)
}

/// `Extractor#handle_struct`: one line while inside a `typedef struct`.
fn handle_struct(
    line: &str,
    current: &mut Option<StructDef>,
    out: &mut Vec<StructDef>,
    open_comment: &mut bool,
) {
    static FIELD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^\s+(struct |const )?([A-z0-9]+)\s+(\*){0,2}([A-z_]+)(?:\s+pg_node_attr\(\w+\))?;\s*(/\*.+)?",
        )
        .unwrap()
    });
    static CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\}(\s+([A-z]+))?;").unwrap());

    let struct_def = current.as_mut().expect("inside a struct");
    if let Some(captures) = FIELD.captures(line) {
        let c_type = format!(
            "{}{}",
            captures.get(2).map(|m| m.as_str()).unwrap_or_default(),
            captures.get(3).map(|m| m.as_str()).unwrap_or_default()
        );
        let name = captures.get(4).expect("field name").as_str().to_string();
        struct_def.fields.push(FieldDef {
            name: Some(name),
            c_type: Some(c_type),
        });
        *open_comment = line.contains("/*") && !line.contains("*/");
    } else if CLOSE.is_match(line) {
        let struct_def = current.take().expect("inside a struct");
        out.push(struct_def);
        *open_comment = false;
    } else if line.trim_start().starts_with("/*") {
        struct_def.fields.push(FieldDef {
            name: None,
            c_type: None,
        });
        *open_comment = !line.contains("*/");
    } else if *open_comment {
        *open_comment = !line.contains("*/");
    } else if !struct_def.fields.is_empty() {
        struct_def.fields.push(FieldDef {
            name: None,
            c_type: None,
        });
    }
}

/// `Extractor#handle_enum`: one line while inside a `typedef enum`.
fn handle_enum(
    line: &str,
    current: &mut Option<EnumDef>,
    out: &mut Vec<EnumDef>,
    open_comment: &mut bool,
) {
    static VALUE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^\s+([A-z0-9_]+)(?: = (?:(\d+)(?: << (\d+))?|(PG_INT32_MAX)|(?:'(\w)')))?,?\s*((?:[A-z0-9_]+,?\s*)+)?(/\*.+)?",
        )
        .unwrap()
    });
    static CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\}\s*([A-z]+)?;").unwrap());

    let enum_def = current.as_mut().expect("inside an enum");
    if let Some(captures) = VALUE.captures(line) {
        let name = captures.get(1).expect("enumerator").as_str().to_string();
        let last = enum_def.values.iter().filter_map(|v| v.value).next_back();
        let value = if let Some(decimal) = captures.get(2) {
            let decimal: i64 = decimal.as_str().parse().expect("decimal enumerator");
            match captures.get(3) {
                Some(shift) => decimal << shift.as_str().parse::<i64>().expect("shift"),
                None => decimal,
            }
        } else if captures.get(4).is_some() {
            0x7FFF_FFFF
        } else if let Some(ch) = captures.get(5) {
            ch.as_str().chars().next().expect("character") as i64
        } else if let Some(last) = last {
            last + 1
        } else {
            0
        };
        enum_def.values.push(EnumValue {
            name: Some(name),
            value: Some(value),
        });
        if let Some(trailing) = captures.get(6) {
            for name in trailing.as_str().split(',') {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                enum_def.values.push(EnumValue {
                    name: Some(name.to_string()),
                    value: None,
                });
            }
        }
        *open_comment = line.contains("/*") && !line.contains("*/");
    } else if let Some(captures) = CLOSE.captures(line) {
        let mut enum_def = current.take().expect("inside an enum");
        if enum_def.name.is_none() {
            enum_def.name = captures.get(1).map(|m| m.as_str().to_string());
        }
        out.push(enum_def);
        *open_comment = false;
    } else if line.trim_start().starts_with("/*") || *open_comment {
        enum_def.values.push(EnumValue {
            name: None,
            value: None,
        });
        *open_comment = !line.contains("*/");
    } else {
        enum_def.values.push(EnumValue {
            name: None,
            value: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_values_resolve_like_the_ruby_script() {
        let def = EnumDef {
            name: Some("X".into()),
            values: vec![
                EnumValue {
                    name: Some("A".into()),
                    value: None,
                },
                EnumValue {
                    name: Some("B".into()),
                    value: Some(4),
                },
                EnumValue {
                    name: Some("C".into()),
                    value: None,
                },
                EnumValue {
                    name: Some("D".into()),
                    value: Some(1 << 5),
                },
            ],
        };
        assert_eq!(
            def.resolved_values(),
            vec![
                ("A".to_string(), 0),
                ("B".to_string(), 4),
                ("C".to_string(), 5),
                ("D".to_string(), 32),
            ]
        );
    }
}
