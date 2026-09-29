//! Encoding conversions (conversioncmds.c): CREATE CONVERSION names two
//! existing encodings and a conversion function with the fixed signature,
//! conversion names are unique per schema.

use pg_query::protobuf::CreateConversionStmt;

use super::DdlError;
use super::util::node_string;
use crate::oid::{PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::PgCatalog;

/// pg_encname_tbl (encnames.c): every accepted spelling, cleaned, with
/// its encoding's canonical name.
const ENCODING_NAMES: &[(&str, &str)] = &[
    ("abc", "WIN1258"),
    ("alt", "WIN866"),
    ("big5", "BIG5"),
    ("euccn", "EUC_CN"),
    ("eucjis2004", "EUC_JIS_2004"),
    ("eucjp", "EUC_JP"),
    ("euckr", "EUC_KR"),
    ("euctw", "EUC_TW"),
    ("gb18030", "GB18030"),
    ("gbk", "GBK"),
    ("iso88591", "LATIN1"),
    ("iso885910", "LATIN6"),
    ("iso885913", "LATIN7"),
    ("iso885914", "LATIN8"),
    ("iso885915", "LATIN9"),
    ("iso885916", "LATIN10"),
    ("iso88592", "LATIN2"),
    ("iso88593", "LATIN3"),
    ("iso88594", "LATIN4"),
    ("iso88595", "ISO_8859_5"),
    ("iso88596", "ISO_8859_6"),
    ("iso88597", "ISO_8859_7"),
    ("iso88598", "ISO_8859_8"),
    ("iso88599", "LATIN5"),
    ("johab", "JOHAB"),
    ("koi8", "KOI8R"),
    ("koi8r", "KOI8R"),
    ("koi8u", "KOI8U"),
    ("latin1", "LATIN1"),
    ("latin10", "LATIN10"),
    ("latin2", "LATIN2"),
    ("latin3", "LATIN3"),
    ("latin4", "LATIN4"),
    ("latin5", "LATIN5"),
    ("latin6", "LATIN6"),
    ("latin7", "LATIN7"),
    ("latin8", "LATIN8"),
    ("latin9", "LATIN9"),
    ("mskanji", "SJIS"),
    ("muleinternal", "MULE_INTERNAL"),
    ("shiftjis", "SJIS"),
    ("shiftjis2004", "SHIFT_JIS_2004"),
    ("sjis", "SJIS"),
    ("sqlascii", "SQL_ASCII"),
    ("tcvn", "WIN1258"),
    ("tcvn5712", "WIN1258"),
    ("uhc", "UHC"),
    ("unicode", "UTF8"),
    ("utf8", "UTF8"),
    ("vscii", "WIN1258"),
    ("win", "WIN1251"),
    ("win1250", "WIN1250"),
    ("win1251", "WIN1251"),
    ("win1252", "WIN1252"),
    ("win1253", "WIN1253"),
    ("win1254", "WIN1254"),
    ("win1255", "WIN1255"),
    ("win1256", "WIN1256"),
    ("win1257", "WIN1257"),
    ("win1258", "WIN1258"),
    ("win866", "WIN866"),
    ("win874", "WIN874"),
    ("win932", "SJIS"),
    ("win936", "GBK"),
    ("win949", "UHC"),
    ("win950", "BIG5"),
    ("windows1250", "WIN1250"),
    ("windows1251", "WIN1251"),
    ("windows1252", "WIN1252"),
    ("windows1253", "WIN1253"),
    ("windows1254", "WIN1254"),
    ("windows1255", "WIN1255"),
    ("windows1256", "WIN1256"),
    ("windows1257", "WIN1257"),
    ("windows1258", "WIN1258"),
    ("windows866", "WIN866"),
    ("windows874", "WIN874"),
    ("windows932", "SJIS"),
    ("windows936", "GBK"),
    ("windows949", "UHC"),
    ("windows950", "BIG5"),
];

/// pg_char_to_encoding: the canonical name of the encoding `name` spells
/// (clean_encoding_name keeps the lowercased alphanumerics).
fn encoding(name: &str) -> Option<&'static str> {
    let clean: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    ENCODING_NAMES
        .iter()
        .find(|(alias, _)| *alias == clean)
        .map(|(_, canonical)| *canonical)
}

/// `(int4, int4, cstring, internal, int4, bool)`.
const SIGNATURE: [u32; 6] = [23, 23, 2275, 2281, 23, 16];

pub fn create_conversion(
    interp: &mut PgCatalog,
    stmt: &CreateConversionStmt,
) -> Result<(), DdlError> {
    let (nsoid, name) = super::util::ensure_qualified_name(interp, &stmt.conversion_name)?;
    let Some(from) = encoding(&stmt.for_encoding_name) else {
        return Err(DdlError::TypeNotFound(format!(
            "source encoding \"{}\" does not exist",
            stmt.for_encoding_name
        )));
    };
    let Some(to) = encoding(&stmt.to_encoding_name) else {
        return Err(DdlError::TypeNotFound(format!(
            "destination encoding \"{}\" does not exist",
            stmt.to_encoding_name
        )));
    };
    // SQL_ASCII conversions would be no-ops.
    if from == "SQL_ASCII" || to == "SQL_ASCII" {
        return Err(DdlError::Parse(
            "encoding conversion to or from \"SQL_ASCII\" is not supported".into(),
        ));
    }
    let parts: Vec<&str> = stmt.func_name.iter().filter_map(node_string).collect();
    let (schema, func) = match parts.as_slice() {
        [schema, func] => (Some(*schema), *func),
        [func] => (None, *func),
        _ => return Ok(()),
    };
    let wanted: Vec<PgTypeOid> = SIGNATURE.iter().map(|&o| PgTypeOid::from_raw(o)).collect();
    // FindDefaultConversionProc / LookupFuncName with the fixed argument
    // types.
    let Some(proc) = interp
        .find_functions(schema, func)
        .into_iter()
        .find(|p| p.proargtypes == wanted)
    else {
        return Err(DdlError::TypeNotFound(format!(
            "function {func}(integer, integer, cstring, internal, integer, boolean) does not \
             exist"
        )));
    };
    if proc.prorettype != crate::pg_catalog::oid::INT4 {
        return Err(DdlError::Parse(format!(
            "encoding conversion function {func} must return type integer"
        )));
    }
    // CreateConversionCommand calls the function once, and a built-in C
    // conversion function rejects encodings it doesn't implement.
    if interp.namespace_name(proc.pronamespace) == Some("pg_catalog")
        && let Some((_, pairs)) = super::conversion_procs::ACCEPTED_PAIRS
            .iter()
            .find(|(n, _)| *n == proc.proname)
        && !pairs.contains(&(from, to))
    {
        return Err(DdlError::Parse(conversion_rejection(
            &proc.proname,
            pairs,
            from,
            to,
        )));
    }
    if interp
        .conversions
        .iter()
        .any(|(n, ns)| *n == name && *ns == nsoid)
    {
        return Err(DdlError::DuplicateObject(format!(
            "conversion \"{name}\" already exists"
        )));
    }
    interp.conversions.push((name, nsoid));
    Ok(())
}

/// The error a built-in conversion function raises for an encoding pair it
/// doesn't accept: `CHECK_ENCODING_CONVERSION_ARGS` checks a fixed source,
/// then a fixed destination; the ISO 8859 / WIN family functions then look
/// the family member up (`unexpected encoding ID N for … character sets`).
fn conversion_rejection(proname: &str, pairs: &[(&str, &str)], from: &str, to: &str) -> String {
    let sources: Vec<&str> = pairs.iter().map(|&(s, _)| s).collect();
    let destinations: Vec<&str> = pairs.iter().map(|&(_, d)| d).collect();
    fn fixed<'a>(side: &[&'a str]) -> Option<&'a str> {
        side.iter().all(|&e| e == side[0]).then_some(side[0])
    }
    if let Some(src) = fixed(&sources)
        && src != from
    {
        return format!("expected source encoding \"{src}\", but got \"{from}\"");
    }
    if let Some(dst) = fixed(&destinations)
        && dst != to
    {
        return format!("expected destination encoding \"{dst}\", but got \"{to}\"");
    }
    let member = if fixed(&sources).is_none() { from } else { to };
    let id = super::conversion_procs::ENCODING_IDS
        .iter()
        .find(|(n, _)| *n == member)
        .map_or(0, |&(_, id)| id);
    let family = if proname.contains("iso8859") {
        "ISO 8859"
    } else {
        "WIN"
    };
    format!("unexpected encoding ID {id} for {family} character sets")
}

fn find(interp: &PgCatalog, names: &[&str]) -> Option<(String, PgNamespaceOid)> {
    let (schema, name) = match names {
        [schema, name] => (Some(*schema), *name),
        [name] => (None, *name),
        _ => return None,
    };
    interp
        .schemas_for_lookup(schema)
        .into_iter()
        .find_map(|ns| {
            interp
                .conversions
                .iter()
                .find(|(n, s)| n == name && *s == ns)
                .cloned()
        })
}

/// DROP CONVERSION [IF EXISTS] name.
pub(crate) fn drop_conversion(
    interp: &mut PgCatalog,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(pg_query::protobuf::node::Node::List(l)) = obj_node.node.as_ref() else {
        return Ok(());
    };
    let names: Vec<&str> = l.items.iter().filter_map(node_string).collect();
    match find(interp, &names) {
        Some(found) => {
            interp.conversions.retain(|c| *c != found);
            Ok(())
        }
        None if missing_ok => Ok(()),
        None => Err(DdlError::TypeNotFound(format!(
            "conversion \"{}\" does not exist",
            names.join(".")
        ))),
    }
}

/// ALTER CONVERSION name RENAME TO new.
pub(crate) fn rename_conversion(
    interp: &mut PgCatalog,
    stmt: &pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(pg_query::protobuf::node::Node::List(l)) =
        stmt.object.as_deref().and_then(|o| o.node.as_ref())
    else {
        return Ok(());
    };
    let names: Vec<&str> = l.items.iter().filter_map(node_string).collect();
    let Some((old, ns)) = find(interp, &names) else {
        return Err(DdlError::TypeNotFound(format!(
            "conversion \"{}\" does not exist",
            names.join(".")
        )));
    };
    if interp
        .conversions
        .iter()
        .any(|(n, s)| *n == stmt.newname && *s == ns)
    {
        let schema = interp.namespace_name(ns).unwrap_or("?").to_owned();
        return Err(DdlError::DuplicateObject(format!(
            "conversion \"{}\" already exists in schema \"{schema}\"",
            stmt.newname
        )));
    }
    for c in interp
        .conversions
        .iter_mut()
        .filter(|(n, s)| *n == old && *s == ns)
    {
        c.0.clone_from(&stmt.newname);
    }
    Ok(())
}
