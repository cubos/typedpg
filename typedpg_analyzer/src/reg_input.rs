//! Input of the object-identifier types `regclass`, `regproc`, `regtype`, …
//! (regproc.c) — what an untyped literal coerced to one of them runs at
//! parse time. Each input function takes an OID (all digits) or `-`, and
//! otherwise parses the value as a name and looks it up:
//!
//! - most split a possibly qualified identifier with
//!   `stringToQualifiedNameList` (SplitIdentifierString: quoting, `""`
//!   escapes, downcasing, surrounding whitespace, truncation to 63 bytes)
//!   and fail with `invalid name syntax` when it doesn't split;
//! - `regtype` parses a whole type name with the grammar's type-name mode
//!   (`parseTypeString`), so its errors are the grammar's;
//! - `regprocedure` / `regoperator` parse `name(argtypes)`
//!   (`parseNameAndArgTypes`).
//!
//! Like [`crate::literal_input::validate`] this must never reject a value
//! PG accepts: lookups of objects the snapshot doesn't model (roles, system
//! relations outside the seed) accept.

use crate::pg_catalog::PgCatalog;

/// PG's `NAMEDATALEN - 1`: identifiers are truncated to 63 bytes.
const MAX_IDENTIFIER_LEN: usize = 63;

/// Validate `content` as input to the reg* type `typname`.
pub(crate) fn validate(typname: &str, content: &str, snapshot: &PgCatalog) -> Result<(), String> {
    // parseDashOrOid: `-` is InvalidOid; an all-digit value is an OID,
    // subject to oidin's range check.
    if content == "-" {
        return Ok(());
    }
    if !content.is_empty() && content.bytes().all(|b| b.is_ascii_digit()) {
        if content.parse::<u64>().is_ok_and(|v| v <= u32::MAX as u64) {
            return Ok(());
        }
        return Err(format!("value \"{content}\" is out of range for type oid"));
    }
    match typname {
        "regtype" => regtype_in(content, snapshot),
        "regprocedure" | "regoperator" => name_and_arg_types(content).map(|_| ()),
        _ => {
            let names = qualified_name_list(content)?;
            lookup_by_names(typname, content, &names, snapshot)
        }
    }
}

fn invalid_name_syntax() -> String {
    "invalid name syntax".to_string()
}

/// `truncate_identifier`: cut to 63 bytes on a character boundary.
fn truncate(mut s: String) -> String {
    if s.len() > MAX_IDENTIFIER_LEN {
        let mut end = MAX_IDENTIFIER_LEN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// `scanner_isspace`.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b')
}

/// `stringToQualifiedNameList` over `SplitIdentifierString(…, '.', …)`:
/// the dot-separated identifiers of `s` — double-quoted ones kept verbatim
/// (with `""` for a quote), others downcased — or `invalid name syntax`.
pub(crate) fn qualified_name_list(s: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let skip_space = |i: &mut usize| {
        while *i < chars.len() && is_space(chars[*i]) {
            *i += 1;
        }
    };
    let mut names = Vec::new();
    skip_space(&mut i);
    if i == chars.len() {
        // An empty list (whitespace only) is not a name.
        return Err(invalid_name_syntax());
    }
    loop {
        let name = if chars.get(i) == Some(&'"') {
            i += 1;
            let mut out = String::new();
            loop {
                match chars.get(i) {
                    None => return Err(invalid_name_syntax()),
                    Some('"') if chars.get(i + 1) == Some(&'"') => {
                        out.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(&c) => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            out
        } else {
            let start = i;
            while i < chars.len() && chars[i] != '.' && !is_space(chars[i]) {
                i += 1;
            }
            if i == start {
                return Err(invalid_name_syntax());
            }
            // downcase_identifier: ASCII only.
            chars[start..i]
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect()
        };
        names.push(truncate(name));
        skip_space(&mut i);
        match chars.get(i) {
            Some('.') => {
                i += 1;
                skip_space(&mut i);
            }
            None => return Ok(names),
            Some(_) => return Err(invalid_name_syntax()),
        }
    }
}

/// A name list in PG's `NameListToString` form (joined, unquoted).
fn joined(names: &[String]) -> String {
    names.join(".")
}

/// Schemas whose relations the snapshot doesn't carry.
fn is_system_schema(schema: &str) -> bool {
    matches!(schema, "pg_catalog" | "information_schema" | "pg_toast")
        || schema.starts_with("pg_temp")
}

/// Look `names` (split from the input `content`) up as a `typname` value.
fn lookup_by_names(
    typname: &str,
    content: &str,
    names: &[String],
    snapshot: &PgCatalog,
) -> Result<(), String> {
    match typname {
        "regclass" => {
            // makeRangeVarFromNameList + RangeVarGetRelid(missing_ok).
            let (schema, rel) = match names {
                [r] => (None, r.as_str()),
                [s, r] => (Some(s.as_str()), r.as_str()),
                [c, s, r] => {
                    // The catalog must be the current database, which the
                    // analyzer can't know: taken as another one.
                    return Err(format!(
                        "cross-database references are not implemented: \"{c}.{s}.{r}\""
                    ));
                }
                _ => {
                    return Err(format!(
                        "improper relation name (too many dotted names): {}",
                        joined(names)
                    ));
                }
            };
            if let Some(class) = snapshot.resolve_table(schema, rel) {
                crate::ddl::depend::note(crate::ddl::depend::ObjectAddress::relation(class.oid));
                return Ok(());
            }
            if schema.is_some_and(is_system_schema) || (schema.is_none() && rel.starts_with("pg_"))
            {
                return Ok(());
            }
            Err(format!("relation \"{}\" does not exist", joined(names)))
        }
        "regproc" | "regoper" => {
            // DeconstructQualifiedName, then FuncnameGetCandidates /
            // OpernameGetCandidates with a missing schema allowed.
            let (schema, name) = deconstruct(names)?;
            if schema.is_some_and(|s| snapshot.namespace_oid(s).is_none()) {
                return Err(not_found(typname, content));
            }
            let count = if typname == "regproc" {
                snapshot.find_functions(schema, name).len()
            } else {
                let path = snapshot.schemas_for_lookup(schema);
                snapshot
                    .pg_operator
                    .values()
                    .filter(|o| o.oprname == name && path.contains(&o.oprnamespace))
                    .count()
            };
            match count {
                0 => Err(not_found(typname, content)),
                1 => Ok(()),
                _ if typname == "regproc" => {
                    Err(format!("more than one function named \"{content}\""))
                }
                _ => Err(format!("more than one operator named {content}")),
            }
        }
        "regnamespace" | "regrole" => {
            let [name] = names else {
                return Err(invalid_name_syntax());
            };
            if typname == "regnamespace"
                && snapshot.namespace_oid(name).is_none()
                && !is_system_schema(name)
            {
                return Err(format!("schema \"{name}\" does not exist"));
            }
            // Roles are not modelled.
            Ok(())
        }
        "regcollation" => {
            let (schema, name) = deconstruct(names)?;
            if schema.is_some_and(|s| snapshot.namespace_oid(s).is_none())
                || snapshot.resolve_collation(schema, name).is_none()
            {
                return Err(format!(
                    "collation \"{}\" for encoding \"UTF8\" does not exist",
                    joined(names)
                ));
            }
            Ok(())
        }
        "regconfig" | "regdictionary" => {
            let (schema, _) = deconstruct(names)?;
            let what = if typname == "regconfig" {
                "text search configuration"
            } else {
                "text search dictionary"
            };
            if schema.is_some_and(|s| snapshot.namespace_oid(s).is_none()) {
                return Err(format!("{what} \"{}\" does not exist", joined(names)));
            }
            let parts: Vec<&str> = names.iter().map(String::as_str).collect();
            let kind = if typname == "regconfig" { "c" } else { "d" };
            crate::ddl::text_search::find(snapshot, kind, &parts).map_err(|e| e.to_string())?;
            let (schema, name) = deconstruct(names)?;
            crate::ddl::depend::note_ts_object(snapshot, kind, schema, name);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// regprocin's / regoperin's lookup failure, which names the input as
/// written (`' EMPTY '`), not the identifier it split into.
fn not_found(typname: &str, content: &str) -> String {
    if typname == "regproc" {
        format!("function \"{content}\" does not exist")
    } else {
        format!("operator does not exist: {content}")
    }
}

/// `DeconstructQualifiedName`: `name`, `schema.name` or
/// `catalog.schema.name` (another database, see above).
fn deconstruct(names: &[String]) -> Result<(Option<&str>, &str), String> {
    match names {
        [n] => Ok((None, n)),
        [s, n] => Ok((Some(s), n)),
        [_, _, _] => Err(format!(
            "cross-database references are not implemented: {}",
            joined(names)
        )),
        _ => Err(format!(
            "improper qualified name (too many dotted names): {}",
            joined(names)
        )),
    }
}

/// `regtypein` → `parseTypeString`: the value must parse as a lone type
/// name (the grammar's own errors otherwise), not `SETOF`, and name an
/// existing type with a valid modifier.
fn regtype_in(content: &str, snapshot: &PgCatalog) -> Result<(), String> {
    use typedpg_pg_query::protobuf::node;
    // typeStringToTypeName fails an empty / all-whitespace value before
    // the grammar sees it.
    if content
        .chars()
        .all(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b'))
    {
        return Err(format!("invalid type name \"{content}\""));
    }
    // The grammar's type-name mode decides whether the text is a lone type
    // name (and words its syntax errors) ...
    typedpg_pg_query::parse_type_name(content).map_err(|e| match e {
        typedpg_pg_query::Error::Parse { message, .. } => message,
        other => other.to_string(),
    })?;
    // ... but libpg_query's tree output for that mode drops the TypeName's
    // details, so read them from the same text in cast position, where the
    // grammar takes the same `Typename` — except `SETOF`, which only the
    // type-name production admits (parseTypeString rejects it).
    let Ok(parsed) = typedpg_pg_query::parse(&format!("SELECT NULL::{content}")) else {
        return Err(format!("invalid type name \"{content}\""));
    };
    // Anything but exactly `NULL::<type>` there (`setof int` reads as a
    // cast to a type `setof` aliased `int`) is a SETOF type name.
    let tn = match parsed.protobuf.stmts.as_slice() {
        [stmt] => match stmt.stmt.as_deref().and_then(|n| n.node.as_ref()) {
            Some(node::Node::SelectStmt(sel))
                if sel.from_clause.is_empty()
                    && sel.where_clause.is_none()
                    && sel.into_clause.is_none() =>
            {
                match sel.target_list.as_slice() {
                    [t] => match t.node.as_ref() {
                        Some(node::Node::ResTarget(rt)) if rt.name.is_empty() => {
                            match rt.val.as_deref().and_then(|v| v.node.as_ref()) {
                                Some(node::Node::TypeCast(c)) => c.type_name.as_ref(),
                                _ => None,
                            }
                        }
                        _ => None,
                    },
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    };
    let Some(tn) = tn.filter(|tn| !tn.setof) else {
        return Err(format!("invalid type name \"{content}\""));
    };
    // LookupTypeName → DeconstructQualifiedName.
    let names = crate::expr::extract_string_fields(&tn.names);
    if names.len() > 2 {
        deconstruct(&names)?;
    }
    let t = crate::ddl::util::lookup_type_name(tn, snapshot).map_err(|e| match e {
        crate::ddl::DdlError::TypeNotFound(msg) | crate::ddl::DdlError::Parse(msg) => msg,
        other => other.to_string(),
    })?;
    crate::typmod::encode(snapshot, t, &tn.typmods)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `parseNameAndArgTypes`: `name(type, …)` — the parentheses are checked
/// and the name must split; the argument types and the lookup are not
/// modelled (accepted).
fn name_and_arg_types(content: &str) -> Result<(), String> {
    // The first `(` outside double quotes.
    let mut in_quote = false;
    let mut open = None;
    for (i, c) in content.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            '(' if !in_quote => {
                open = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(open) = open else {
        return Err("expected a left parenthesis".to_string());
    };
    let rest = content[open + 1..].trim_end_matches(is_space);
    if !rest.ends_with(')') {
        return Err("expected a right parenthesis".to_string());
    }
    let names = qualified_name_list(&content[..open])?;
    if names.len() == 3 {
        return Err(format!(
            "cross-database references are not implemented: {}",
            joined(&names)
        ));
    }
    if names.len() > 3 {
        return Err(format!(
            "improper qualified name (too many dotted names): {}",
            joined(&names)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::qualified_name_list;

    #[test]
    fn splits_identifiers_like_split_identifier_string() {
        assert_eq!(qualified_name_list(" Pg_Class ").unwrap(), ["pg_class"]);
        assert_eq!(
            qualified_name_list("public . \"My\"\"T\"").unwrap(),
            ["public", "My\"T"]
        );
        assert_eq!(qualified_name_list("\"\"").unwrap(), [""]);
        assert_eq!(qualified_name_list("1.5e3").unwrap(), ["1", "5e3"]);
        for bad in ["", "  ", "a..b", "a.", "x y", "\"a", "a b.c"] {
            assert!(qualified_name_list(bad).is_err(), "{bad:?}");
        }
    }
}
