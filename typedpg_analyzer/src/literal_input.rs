//! Parse-time validation of untyped string literals against a target type.
//!
//! When PostgreSQL coerces an `unknown` *constant* to a concrete type (an
//! explicit `'x'::T` cast, an operator/function argument, a CASE/COALESCE
//! branch, an INSERT/UPDATE assignment, a WHERE/LIMIT clause, …) it runs the
//! type's input function immediately, at parse-analysis time — so `'x'::int`
//! fails `prepare` with `invalid input syntax for type integer: "x"`. The
//! static analyzer mirrors that here for the types whose input grammar is
//! small and stable enough to model exactly.
//!
//! The contract is **conservative**: [`validate`] must never reject a string
//! PostgreSQL's input function would accept. Types whose grammar we don't
//! model (datetime values, geometric coordinates, inet, …) are accepted
//! wholesale — except for the empty string, which a fixed list of them is
//! known to reject (see [`EMPTY_INVALID`]). When in doubt, accept.
//!
//! Each rejection carries PG's message verbatim (the analyzer's error-message
//! contract): `invalid input syntax for type %s: "%s"`, `value "%s" is out of
//! range for type %s`, `malformed array literal: "%s"`, `malformed range
//! literal: "%s"`, `invalid input value for enum %s: "%s"`, or `invalid name
//! syntax` — all verified against PostgreSQL 18.

use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, TypCategory, TypType, oid};

/// Outcome of validating literal `content` against `target`: `Ok(())` when PG
/// would accept it (or we can't tell), `Err(message)` with PG's verbatim
/// parse-time error when it provably wouldn't.
pub(crate) fn validate(
    content: &str,
    target: PgTypeOid,
    snapshot: &PgCatalog,
) -> Result<(), String> {
    // Domain values are validated by the *base* type's input function, and
    // PG's message names the base type (`'x'::posint` → `… for type integer`).
    let target = snapshot.unwrap_domain(target);

    match target {
        oid::BOOL => return validate_bool(content),
        oid::INT2 => return validate_int(content, i16::MIN as i128, i16::MAX as i128, "smallint"),
        oid::INT4 => return validate_int(content, i32::MIN as i128, i32::MAX as i128, "integer"),
        oid::INT8 => return validate_int(content, i64::MIN as i128, i64::MAX as i128, "bigint"),
        oid::OID => return validate_oid(content),
        oid::FLOAT4 => return validate_float(content, "real"),
        oid::FLOAT8 => return validate_float(content, "double precision"),
        oid::NUMERIC => return validate_numeric(content),
        _ => {}
    }

    let Some(t) = snapshot.get_type(target) else {
        return Ok(());
    };

    // Enums: exact label match (no whitespace trimming, case-sensitive).
    if t.typtype == TypType::Enum {
        if snapshot.enum_labels_of(target).contains(&content) {
            return Ok(());
        }
        // PG renders the enum's name search-path aware (`st`, `s2.en2`).
        let name = crate::ddl::util::format_type_for_message(snapshot, target);
        return Err(format!(
            "invalid input value for enum {name}: \"{content}\""
        ));
    }

    // Ranges: must be `empty` (case-insensitive, surrounding whitespace
    // allowed) or start with `(` / `[`. Bound contents are not validated.
    if t.typtype == TypType::Range {
        let trimmed = content.trim_matches(|c: char| c.is_ascii_whitespace());
        if trimmed.eq_ignore_ascii_case("empty") || trimmed.starts_with(['(', '[']) {
            return Ok(());
        }
        return Err(format!("malformed range literal: \"{content}\""));
    }

    // Multiranges: the value must open with `{` after optional whitespace
    // (`'{}'` is the valid empty multirange). Member ranges aren't validated.
    if t.typtype == TypType::Multirange {
        let trimmed = content.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if trimmed.starts_with('{') {
            return Ok(());
        }
        return Err(format!("malformed multirange literal: \"{content}\""));
    }

    // True arrays: after optional leading whitespace the value must open with
    // `{` (or `[` for the explicit-dimensions form `[1:2]={…}`). Element
    // contents are not validated. `oidvector`/`int2vector` share the Array
    // category but use their own space-separated input format — skip them
    // (they're exactly the types whose element doesn't point back via
    // `typarray`).
    if t.typcategory == TypCategory::Array
        && t.typelem
            .is_some_and(|e| snapshot.array_type_of(e) == Some(target))
    {
        let trimmed = content.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if trimmed.starts_with('{') {
            return Ok(());
        }
        // The explicit-dimensions form is `[lo:hi]…={…}` — a `[` opener
        // without the `={` separator (e.g. `'[1,]'`) is malformed.
        if trimmed.starts_with('[') && trimmed.contains("={") {
            return Ok(());
        }
        return Err(format!("malformed array literal: \"{content}\""));
    }

    // Name-resolving and fixed-syntax pg_catalog builtins, keyed by name.
    if snapshot.namespace_name(t.typnamespace) != Some("pg_catalog") {
        return Ok(());
    }
    match t.typname.as_str() {
        "uuid" => validate_uuid(content),
        "json" | "jsonb" => validate_json(content),
        "jsonpath" => crate::jsonpath_input::validate(content),
        // The object-resolving reg* family parses the value as a (possibly
        // qualified) SQL identifier and resolves it at parse time. An
        // empty/whitespace-only string is never a valid name; for the two
        // members whose target catalog we fully model (relations and
        // functions) a *simple* unquoted name is also resolved here. The
        // rest (regtype's full type-name grammar, roles/collations/text
        // search objects we don't track) are accepted unchecked.
        name @ ("regproc" | "regprocedure" | "regoper" | "regoperator" | "regclass" | "regtype"
        | "regcollation" | "regconfig" | "regdictionary" | "regnamespace" | "regrole") => {
            let trimmed = content.trim_matches(|c: char| c.is_ascii_whitespace());
            if trimmed.is_empty() {
                return Err("invalid name syntax".to_string());
            }
            // All-digits is an OID literal — subject to oid's range check
            // (`'99…9'::regproc` → `value "…" is out of range for type oid`).
            // The check is on the *raw* content: reg* input functions only
            // take the OID path when the whole string is digits.
            if content.chars().all(|c| c.is_ascii_digit()) {
                if content.parse::<u64>().is_ok_and(|v| v <= u32::MAX as u64) {
                    return Ok(());
                }
                return Err(format!("value \"{content}\" is out of range for type oid"));
            }
            // Surrounding whitespace interacts with each reg* type's own
            // trimming rules (`' 42 '::regproc` is rejected, `' users '` is
            // not necessarily) — accept rather than model them.
            if trimmed != content {
                return Ok(());
            }
            if !matches!(name, "regclass" | "regproc" | "regtype") {
                return Ok(());
            }
            // Quoted / qualified forms need real identifier parsing — skip.
            if trimmed.contains(['"', '.']) {
                return Ok(());
            }
            // `regtype` parses its value with the full type-name grammar
            // (`'character varying'`, `'int[]'`, `'numeric(10,2)'`, …); only
            // a single bare identifier is simple enough to resolve here.
            if name == "regtype"
                && !trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Ok(());
            }
            // An unquoted name containing whitespace fails PG's identifier
            // splitting up front (`'1 day'::regclass` → invalid name syntax).
            if name != "regtype" && trimmed.chars().any(|c| c.is_ascii_whitespace()) {
                return Err("invalid name syntax".to_string());
            }
            // Unquoted identifiers fold to lowercase before lookup. System
            // objects (`pg_*`, information_schema) may be absent from the
            // snapshot — accept those rather than risk a false rejection.
            let folded = trimmed.to_ascii_lowercase();
            if folded.starts_with("pg_") || folded.starts_with("information_schema") {
                return Ok(());
            }
            match name {
                "regclass" if snapshot.resolve_table(None, &folded).is_none() => {
                    Err(format!("relation \"{folded}\" does not exist"))
                }
                // `regproc` (unlike `regprocedure`) requires the bare name to
                // resolve to exactly one function.
                "regproc" => match snapshot.find_functions(None, &folded).len() {
                    0 => Err(format!("function \"{folded}\" does not exist")),
                    1 => Ok(()),
                    _ => Err(format!("more than one function named \"{folded}\"")),
                },
                // A bare identifier for `regtype`: try the SQL-standard
                // aliases (`integer` → `int4`) then the catalog.
                "regtype" => {
                    let normalized = crate::ddl::util::normalize_type_name(&folded);
                    if snapshot.resolve_type_by_name(None, normalized).is_some()
                        || snapshot.resolve_type_by_name(None, &folded).is_some()
                    {
                        Ok(())
                    } else {
                        Err(format!("type \"{folded}\" does not exist"))
                    }
                }
                _ => Ok(()),
            }
        }
        // Datetime family: the full input grammar is out of scope, but two
        // slices are exactly checkable (verified against PG 18): the empty
        // string, and purely alphabetic tokens — PG's datetime lexer only
        // accepts those when they're one of the special keywords (`now`,
        // `today`, `epoch`, `infinity`, …); any other bare word is
        // `invalid input syntax`. Anything with digits or punctuation
        // (which can route to *other* messages, e.g. `time zone "a.m." not
        // recognized`, or be a valid value like `'now()'`) is accepted
        // unchecked. The message uses the input function's own type-name
        // string, which differs from `format_type` for the timestamp family.
        name @ ("date" | "time" | "timetz" | "timestamp" | "timestamptz" | "interval") => {
            let msg_name = match name {
                "timetz" => "time with time zone",
                "timestamptz" => "timestamp with time zone",
                other => other,
            };
            let keywords: &[&str] = match name {
                "time" | "timetz" => &["now", "allballs"],
                "interval" => &["infinity"],
                _ => &["now", "today", "tomorrow", "yesterday", "epoch", "infinity"],
            };
            let trimmed = content
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .to_ascii_lowercase();
            // `'+infinity'` / `'-infinity'` are valid wherever `infinity` is.
            let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(&trimmed);
            if keywords.contains(&unsigned) {
                return Ok(());
            }
            let purely_alphabetic = !trimmed.is_empty()
                && trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphabetic() || c.is_ascii_whitespace());
            if trimmed.is_empty() || purely_alphabetic {
                return Err(crate::pgmsg::invalid_input_syntax_for_type(
                    msg_name, content,
                ));
            }
            // PG's datetime tokenizer (`ParseDateTime`, datetime.c) accepts
            // letters, digits, whitespace and the delimiter set used by
            // dates/times/zones — any other character is an immediate
            // DTERR_BAD_FORMAT, before field decoding even starts. (The
            // alphabet includes `/` and `_` for zone names like
            // `America/New_York`, `@` for the interval `ago` syntax, and
            // `()` — `'now()'` is accepted.)
            let tokenizer_ok = content.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || c.is_ascii_whitespace()
                    || matches!(c, ':' | '+' | '-' | '/' | '.' | ',' | '@' | '_' | '(' | ')')
            });
            if !tokenizer_ok {
                return Err(crate::pgmsg::invalid_input_syntax_for_type(
                    msg_name, content,
                ));
            }
            validate_datetime_token(&trimmed, name, msg_name, content)
        }
        // Internal statistics / parse-tree types whose input functions
        // unconditionally refuse input. The message string is the input
        // function's own (note `pg_brin_minmax_multi_summary`'s drops the
        // prefix) — verified against PG 18.
        name @ ("pg_node_tree"
        | "pg_ndistinct"
        | "pg_dependencies"
        | "pg_mcv_list"
        | "pg_brin_bloom_summary"
        | "pg_brin_minmax_multi_summary"
        | "pg_ddl_command") => {
            let msg_name = match name {
                "pg_brin_minmax_multi_summary" => "brin_minmax_multi_summary",
                other => other,
            };
            Err(format!("cannot accept a value of type {msg_name}"))
        }
        "bit" | "varbit" => validate_bit(content),
        "money" => validate_money(content),
        "inet" => validate_inet(content, false),
        "cidr" => validate_inet(content, true),
        "macaddr" => validate_macaddr(content, false),
        "macaddr8" => validate_macaddr(content, true),
        name @ ("point" | "lseg" | "box" | "path" | "polygon" | "circle" | "line") => {
            validate_geometric(content, name)
        }
        "tid" => validate_tid(content),
        "pg_lsn" => validate_pg_lsn(content),
        name @ ("xid" | "xid8" | "cid") => validate_xid(content, name),
        _ => Ok(()),
    }
}

// ─── boolean ────────────────────────────────────────────────────────────────

/// Mirrors `parse_bool_with_len` (bool.c): case-insensitive prefixes of
/// `true`/`false`/`yes`/`no`, the exact-prefix family of `on`/`off` (where a
/// bare `o` is ambiguous and rejected), and single-character `1`/`0`.
/// Surrounding ASCII whitespace is trimmed.
fn validate_bool(content: &str) -> Result<(), String> {
    let v = content
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_ascii_lowercase();
    let ok = match v.as_str() {
        "1" | "0" | "on" => true,
        _ if !v.is_empty() && ("true".starts_with(&v) || "false".starts_with(&v)) => true,
        _ if !v.is_empty() && ("yes".starts_with(&v) || "no".starts_with(&v)) => true,
        // `off` prefixes need length ≥ 2 (`o` alone is ambiguous with `on`).
        _ if v.len() >= 2 && "off".starts_with(&v) => true,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(crate::pgmsg::invalid_input_syntax_for_type(
            "boolean", content,
        ))
    }
}

// ─── integers ───────────────────────────────────────────────────────────────

/// Digit-run parser shared by the integer/numeric validators: a non-empty
/// sequence of digits from `set`, with single underscores allowed *between*
/// digits (PG 16+). Returns the digits (underscores stripped) or `None` on a
/// malformed run; `rest` is left positioned after the run.
fn take_digits(s: &mut &str, radix: u32) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.char_indices().peekable();
    let mut last_was_digit = false;
    let mut end = 0;
    while let Some(&(i, c)) = chars.peek() {
        if c.is_digit(radix) {
            out.push(c);
            last_was_digit = true;
            chars.next();
            end = i + c.len_utf8();
        } else if c == '_' && last_was_digit {
            // A `_` must be followed by another digit (no trailing or double
            // underscores).
            chars.next();
            match chars.peek() {
                Some(&(_, d)) if d.is_digit(radix) => {
                    last_was_digit = false; // consumed on next loop turn
                }
                _ => return None,
            }
        } else {
            break;
        }
    }
    if out.is_empty() {
        return None;
    }
    *s = &s[end..];
    Some(out)
}

/// Mirrors `pg_strtointNN` (numutils.c): optional surrounding whitespace, an
/// optional sign, then decimal digits or a `0x`/`0o`/`0b` radix prefix —
/// underscores allowed between digits — followed by a range check.
fn validate_int(content: &str, min: i128, max: i128, type_name: &str) -> Result<(), String> {
    let syntax_err = || crate::pgmsg::invalid_input_syntax_for_type(type_name, content);
    let mut s = content.trim_matches(|c: char| c.is_ascii_whitespace());

    let negative = match s.as_bytes().first() {
        Some(b'-') => {
            s = &s[1..];
            true
        }
        Some(b'+') => {
            s = &s[1..];
            false
        }
        _ => false,
    };

    let (radix, digits) = parse_radix_digits(&mut s).ok_or_else(syntax_err)?;
    if !s.is_empty() {
        return Err(syntax_err());
    }

    match i128::from_str_radix(&digits, radix) {
        Ok(v) => {
            let v = if negative { -v } else { v };
            if v < min || v > max {
                return Err(format!(
                    "value \"{content}\" is out of range for type {type_name}"
                ));
            }
            Ok(())
        }
        // > 38 digits — definitely out of range for any integer type.
        Err(_) => Err(format!(
            "value \"{content}\" is out of range for type {type_name}"
        )),
    }
}

/// The value of an integer written in `pg_strtoint64`'s syntax (numutils.c):
/// optional surrounding whitespace, an optional sign, then decimal digits or
/// a `0x`/`0o`/`0b` radix prefix, underscores allowed between digits.
/// `None` on a syntax error or a magnitude beyond `i128` (so beyond every
/// PG integer type). PG's `make_const` uses this to type the integer-shaped
/// `T_Float` constants the lexer hands over (anything not fitting int4).
pub(crate) fn parse_pg_integer(content: &str) -> Option<i128> {
    let mut s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    let negative = match s.as_bytes().first() {
        Some(b'-') => {
            s = &s[1..];
            true
        }
        Some(b'+') => {
            s = &s[1..];
            false
        }
        _ => false,
    };
    let (radix, digits) = parse_radix_digits(&mut s)?;
    if !s.is_empty() {
        return None;
    }
    let v = i128::from_str_radix(&digits, radix).ok()?;
    Some(if negative { -v } else { v })
}

/// `0x`/`0o`/`0b`-prefixed or decimal digit run (with underscore rules).
/// Returns `(radix, digits)`; leaves `s` positioned after the run.
fn parse_radix_digits(s: &mut &str) -> Option<(u32, String)> {
    let lower = s.as_bytes();
    let radix = if lower.len() >= 2 && lower[0] == b'0' {
        match lower[1] {
            b'x' | b'X' => Some(16),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        }
    } else {
        None
    };
    if let Some(r) = radix {
        *s = &s[2..];
        // After the prefix the first char must be a digit (no `0x_1`).
        let digits = take_digits(s, r)?;
        Some((r, digits))
    } else {
        let digits = take_digits(s, 10)?;
        Some((10, digits))
    }
}

/// Mirrors `uint32in_subr` (numutils.c): `strtoul` semantics — optional
/// whitespace and sign, decimal or `0x`-prefixed digits, **no** underscores
/// or `0o`/`0b`. The range check follows strtoul's wrap-around acceptance:
/// a value is in range when it fits `uint32`, or when it's negative and its
/// magnitude fits `int32` (so `'-1'::oid` is 4294967295 but
/// `'-4294967295'::oid` is out of range) — verified against PG 18.
fn validate_oid(content: &str) -> Result<(), String> {
    let syntax_err = || crate::pgmsg::invalid_input_syntax_for_type("oid", content);
    let mut s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    let negative = match s.strip_prefix(['+', '-']) {
        Some(rest) => {
            let neg = s.starts_with('-');
            s = rest;
            neg
        }
        None => false,
    };
    let (radix, digits) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(rest) => (16, rest),
        None => (10, s),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(syntax_err());
    }
    let in_range = match u64::from_str_radix(digits, radix) {
        Ok(v) if negative => v <= i32::MAX as u64 + 1,
        Ok(v) => v <= u32::MAX as u64,
        Err(_) => false, // > u64 digits — far out of range
    };
    if !in_range {
        return Err(format!("value \"{content}\" is out of range for type oid"));
    }
    Ok(())
}

// ─── floats ─────────────────────────────────────────────────────────────────

/// Mirrors `float8in` / `float4in`, which delegate to `strtod`: optional
/// whitespace and sign, then `inf`/`infinity`/`nan` (case-insensitive), a
/// decimal float (`1`, `.5`, `5.`, `1e3`, `5.e-3`), or a C99 hex float
/// (`0x1F`, `0x1.8p3`). No underscores, no range check (kept conservative).
fn validate_float(content: &str, type_name: &str) -> Result<(), String> {
    let err = || crate::pgmsg::invalid_input_syntax_for_type(type_name, content);
    let mut s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    if let Some(rest) = s.strip_prefix(['+', '-']) {
        s = rest;
    }
    let lower = s.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" || lower == "nan" {
        return Ok(());
    }
    // C99 hex float: 0x H* [. H*] [p [sign] D+] — at least one hex digit
    // overall.
    if let Some(hex) = lower.strip_prefix("0x") {
        let (mantissa, exp) = match hex.split_once('p') {
            Some((m, e)) => (m, Some(e)),
            None => (hex, None),
        };
        let (int_part, frac_part) = match mantissa.split_once('.') {
            Some((i, f)) => (i, f),
            None => (mantissa, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(err());
        }
        if !int_part.chars().all(|c| c.is_ascii_hexdigit())
            || !frac_part.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(err());
        }
        if let Some(e) = exp {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            if e.is_empty() || !e.chars().all(|c| c.is_ascii_digit()) {
                return Err(err());
            }
        }
        return Ok(());
    }
    // Decimal float: D* [. D*] [e [sign] D+] — at least one digit in the
    // mantissa.
    let (mantissa, exp) = match lower.split_once('e') {
        Some((m, e)) => (m, Some(e)),
        None => (lower.as_str(), None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(err());
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(err());
    }
    if let Some(e) = exp {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        if e.is_empty() || !e.chars().all(|c| c.is_ascii_digit()) {
            return Err(err());
        }
    }
    Ok(())
}

// ─── numeric ────────────────────────────────────────────────────────────────

/// Mirrors `numeric_in`: optional whitespace and sign, then `NaN` /
/// `inf[inity]` (case-insensitive), a `0x`/`0o`/`0b` integer, or a decimal
/// value with optional fraction and `e`-exponent — underscores allowed
/// between digits everywhere. No precision limit check.
pub(crate) fn validate_numeric(content: &str) -> Result<(), String> {
    let err = || crate::pgmsg::invalid_input_syntax_for_type("numeric", content);
    let mut s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    if let Some(rest) = s.strip_prefix(['+', '-']) {
        s = rest;
    }
    let lower = s.to_ascii_lowercase();
    if lower == "nan" || lower == "inf" || lower == "infinity" {
        return Ok(());
    }

    // Radix-prefixed integer form.
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'0' && matches!(bytes[1] | 0x20, b'x' | b'o' | b'b') {
        let mut rest = s;
        match parse_radix_digits(&mut rest) {
            Some(_) if rest.is_empty() => return Ok(()),
            _ => return Err(err()),
        }
    }

    // Decimal: D+ [. D*] | . D+, then optional exponent.
    let mut rest = s;
    let int_digits = take_digits(&mut rest, 10);
    let mut any_digit = int_digits.is_some();
    if let Some(r) = rest.strip_prefix('.') {
        rest = r;
        if rest.starts_with(|c: char| c.is_ascii_digit()) {
            if take_digits(&mut rest, 10).is_none() {
                return Err(err());
            }
            any_digit = true;
        }
    }
    if !any_digit {
        return Err(err());
    }
    if let Some(r) = rest.strip_prefix(['e', 'E']) {
        rest = r;
        if let Some(r2) = rest.strip_prefix(['+', '-']) {
            rest = r2;
        }
        if take_digits(&mut rest, 10).is_none() {
            return Err(err());
        }
    }
    if !rest.is_empty() {
        return Err(err());
    }
    Ok(())
}

// ─── uuid ───────────────────────────────────────────────────────────────────

/// Mirrors `uuid_in`: exactly 32 hex digits, optionally wrapped in one pair
/// of braces, with hyphens allowed only on the standard group boundaries
/// (after hex digits 8, 12, 16, 20). No surrounding whitespace.
fn validate_uuid(content: &str) -> Result<(), String> {
    let err = || crate::pgmsg::invalid_input_syntax_for_type("uuid", content);
    let s = match content.strip_prefix('{') {
        Some(rest) => rest.strip_suffix('}').ok_or_else(err)?,
        None => content,
    };
    let mut ndigits = 0u32;
    for c in s.chars() {
        if c == '-' {
            if !matches!(ndigits, 8 | 12 | 16 | 20) {
                return Err(err());
            }
        } else if c.is_ascii_hexdigit() {
            ndigits += 1;
            if ndigits > 32 {
                return Err(err());
            }
        } else {
            return Err(err());
        }
    }
    if ndigits != 32 {
        return Err(err());
    }
    Ok(())
}

// ─── json ───────────────────────────────────────────────────────────────────

/// Structural RFC 8259 validation, mirroring PG's `json_lex`/`parse_json`.
/// `\u` escapes only check for 4 hex digits — the jsonb-only surrogate-pair
/// and `\u0000` restrictions produce *different* PG messages and are
/// deliberately not modeled (accepted). The message carries no content:
/// PG emits a bare `invalid input syntax for type json` (the specifics go in
/// the DETAIL field, which the prefix contract doesn't cover).
fn validate_json(content: &str) -> Result<(), String> {
    let mut p = JsonParser {
        bytes: content.as_bytes(),
        pos: 0,
        depth: 0,
    };
    let ok = (|| {
        p.skip_ws();
        p.value()?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return None;
        }
        Some(())
    })();
    match ok {
        Some(()) => Ok(()),
        None => Err("invalid input syntax for type json".to_string()),
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: u32,
}

impl JsonParser<'_> {
    /// JSON whitespace: space, tab, LF, CR.
    fn skip_ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> Option<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn value(&mut self) -> Option<()> {
        // Beyond any plausible real document the cost/benefit flips; PG's
        // own limit is the stack guard. Accept by consuming the rest.
        if self.depth > 256 {
            self.pos = self.bytes.len();
            return Some(());
        }
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string(),
            b't' => self.keyword(b"true"),
            b'f' => self.keyword(b"false"),
            b'n' => self.keyword(b"null"),
            _ => self.number(),
        }
    }

    fn keyword(&mut self, kw: &[u8]) -> Option<()> {
        if self.bytes[self.pos..].starts_with(kw) {
            self.pos += kw.len();
            Some(())
        } else {
            None
        }
    }

    fn object(&mut self) -> Option<()> {
        self.eat(b'{')?;
        self.depth += 1;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Some(());
        }
        loop {
            self.skip_ws();
            self.string()?;
            self.skip_ws();
            self.eat(b':')?;
            self.skip_ws();
            self.value()?;
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b'}' => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<()> {
        self.eat(b'[')?;
        self.depth += 1;
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Some(());
        }
        loop {
            self.skip_ws();
            self.value()?;
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b']' => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<()> {
        self.eat(b'"')?;
        loop {
            match self.peek()? {
                b'"' => {
                    self.pos += 1;
                    return Some(());
                }
                b'\\' => {
                    self.pos += 1;
                    match self.peek()? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.pos += 1,
                        b'u' => {
                            self.pos += 1;
                            for _ in 0..4 {
                                if !self.peek()?.is_ascii_hexdigit() {
                                    return None;
                                }
                                self.pos += 1;
                            }
                        }
                        _ => return None,
                    }
                }
                // Unescaped control characters are rejected by PG's lexer.
                c if c < 0x20 => return None,
                _ => self.pos += 1,
            }
        }
    }

    fn number(&mut self) -> Option<()> {
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: `0` or [1-9][0-9]* — no leading zeros.
        match self.peek()? {
            b'0' => self.pos += 1,
            b'1'..=b'9' => {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !self.peek()?.is_ascii_digit() {
                return None;
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !self.peek()?.is_ascii_digit() {
                return None;
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        Some(())
    }
}

// ─── datetime single-token decoding ─────────────────────────────────────────

/// Month names, day-of-week names, and the other alphabetic words PG's
/// datetime decoder accepts glued to digits in a single token
/// (`'15jan2024'::date`, `'2024-01-01t00:00:00z'` → token `00z`). An
/// alphabetic run outside this set in a digit-bearing token is
/// DTERR_BAD_FORMAT (`'42abc'`, `'0b101'`, `'1e3'`).
const DATETIME_WORDS: &[&str] = &[
    "jan",
    "feb",
    "mar",
    "apr",
    "may",
    "jun",
    "jul",
    "aug",
    "sep",
    "sept",
    "oct",
    "nov",
    "dec",
    "january",
    "february",
    "march",
    "april",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "am",
    "pm",
    "t",
    "z",
    "j",
    "bc",
    "ad",
    "mon",
    "tue",
    "tues",
    "wed",
    "weds",
    "thu",
    "thur",
    "thurs",
    "fri",
    "sat",
    "sun",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// Decode the *single-token* shapes of the datetime input grammar — a value
/// with no internal separators (`:`/`/`/`,`/whitespace/`@`/parens). Those
/// shapes are small enough to model exactly (verified against PG 18 case by
/// case); anything multi-field is accepted unchecked. `t` is the value
/// trimmed and lowercased by the caller.
fn validate_datetime_token(
    t: &str,
    name: &str,
    msg_name: &str,
    content: &str,
) -> Result<(), String> {
    let syntax = || crate::pgmsg::invalid_input_syntax_for_type(msg_name, content);
    let range = || format!("date/time field value out of range: \"{content}\"");

    // Multi-field values (separators present) are out of scope — accept.
    if t.contains(|c: char| {
        c.is_ascii_whitespace() || matches!(c, ':' | '/' | ',' | '@' | '(' | ')')
    }) {
        return Ok(());
    }

    // PG has no underscore anywhere in datetime values outside zone *names*
    // (which always ride along a date/time field, i.e. a multi-field value).
    if t.contains('_') {
        return Err(syntax());
    }

    if name == "interval" {
        let body = t.strip_prefix(['+', '-']).unwrap_or(t);
        if body.is_empty() {
            return Err(syntax());
        }
        if body.chars().all(|c| c.is_ascii_digit()) {
            // A bare number is seconds; int64 microseconds cap far below
            // 19 digits of seconds.
            if body.trim_start_matches('0').len() >= 19 {
                return Err(format!("interval field value out of range: \"{content}\""));
            }
            return Ok(());
        }
        // Unit-suffixed forms (`1d`, `2h30m`) and decimals are valid;
        // unknown unit letters are rejected by checking the alpha runs.
        if t.starts_with(|c: char| c.is_ascii_digit()) && t.chars().any(|c| c.is_ascii_alphabetic())
        {
            const INTERVAL_UNITS: &[&str] = &[
                "us",
                "ms",
                "s",
                "sec",
                "secs",
                "second",
                "seconds",
                "m",
                "min",
                "mins",
                "minute",
                "minutes",
                "h",
                "hr",
                "hrs",
                "hour",
                "hours",
                "d",
                "day",
                "days",
                "w",
                "week",
                "weeks",
                "mon",
                "mons",
                "month",
                "months",
                "y",
                "yr",
                "yrs",
                "year",
                "years",
                "ago",
                "c",
                "cent",
                "centuries",
                "century",
                "dec",
                "decade",
                "decades",
                "mil",
                "millennium",
                "millennia",
            ];
            for run in alpha_runs(t) {
                if !INTERVAL_UNITS.contains(&run) {
                    return Err(syntax());
                }
            }
        }
        return Ok(());
    }

    // date / time / timetz / timestamp / timestamptz.
    let is_time = matches!(name, "time" | "timetz");

    // A sign-led number is a lone timezone displacement — never a complete
    // value. PG distinguishes a displacement beyond ±15:59 (its tz limit).
    if let Some(body) = t.strip_prefix('+') {
        if !body.is_empty() && body.chars().all(|c| c.is_ascii_digit()) {
            let hh: u32 = match body.len() {
                2 => body.parse().unwrap_or(0),
                4 => body[..2].parse().unwrap_or(0),
                _ => 0,
            };
            if hh >= 16 {
                return Err(format!(
                    "time zone displacement out of range: \"{content}\""
                ));
            }
            return Err(syntax());
        }
        return Ok(());
    }
    if let Some(body) = t.strip_prefix('-') {
        if !body.is_empty() && body.chars().all(|c| c.is_ascii_digit()) {
            return Err(syntax());
        }
        return Ok(());
    }

    // ISO `yyyy-mm-dd` (a year of 3+ digits first): PG's DecodeDate takes
    // the fields as year, month, day whatever the DateStyle, and
    // ValidateDate rejects a month outside 1..12 or a day past the month's
    // end as a field overflow. (Year 0 has its own BC rules — accept.)
    if !is_time
        && let [y, m, d] = t.split('-').collect::<Vec<_>>().as_slice()
        && y.len() >= 3
        && (1..=2).contains(&m.len())
        && (1..=2).contains(&d.len())
        && [y, m, d]
            .iter()
            .all(|f| f.chars().all(|c| c.is_ascii_digit()))
        && let (Ok(y), Ok(m), Ok(d)) = (y.parse::<u32>(), m.parse::<u32>(), d.parse::<u32>())
        && y > 0
    {
        return if valid_month_day(y, m, d) {
            Ok(())
        } else {
            Err(range())
        };
    }

    // Pure digits: a single concatenated field.
    if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
        return if is_time {
            decode_time_field(t, None).map_err(|oor| if oor { range() } else { syntax() })
        } else {
            decode_date_field(t).map_err(|oor| if oor { range() } else { syntax() })
        };
    }

    // One decimal point: `hhmm.frac` / `hhmmss.frac` are valid times;
    // everything else single-dotted is a syntax error. Two or more dots can
    // be a DateStyle-dependent date (`1.2.3`) — accept.
    let dots = t.matches('.').count();
    if dots == 1
        && let Some((int, frac)) = t.split_once('.')
        && int.chars().all(|c| c.is_ascii_digit())
        && frac.chars().all(|c| c.is_ascii_digit())
    {
        if is_time && matches!(int.len(), 4 | 6) && !frac.is_empty() {
            return decode_time_field(int, Some(frac))
                .map_err(|oor| if oor { range() } else { syntax() });
        }
        return Err(syntax());
    }
    if dots >= 2 {
        return Ok(());
    }

    // Digit-led token with alphabetic runs: month/word forms are valid
    // (`15jan2024`); unknown words are DTERR_BAD_FORMAT (`42abc`, `1e3`).
    if t.starts_with(|c: char| c.is_ascii_digit()) && t.chars().any(|c| c.is_ascii_alphabetic()) {
        for run in alpha_runs(t) {
            if !DATETIME_WORDS.contains(&run) {
                return Err(syntax());
            }
        }
    }
    Ok(())
}

/// The maximal alphabetic runs of a token (`15jan2024` → `["jan"]`).
fn alpha_runs(t: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, c) in t.char_indices() {
        if c.is_ascii_alphabetic() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            runs.push(&t[s..i]);
        }
    }
    if let Some(s) = start {
        runs.push(&t[s..]);
    }
    runs
}

/// `hhmm` / `hhmmss` concatenated time fields (PG `DecodeNumberField` +
/// `ValidateTime`): minutes ≤ 59, seconds ≤ 60 (leap), and the total may
/// not exceed 24:00:00. `Err(true)` = field value out of range,
/// `Err(false)` = invalid syntax.
fn decode_time_field(digits: &str, _frac: Option<&str>) -> Result<(), bool> {
    let (hh, mm, ss) = match digits.len() {
        4 => (&digits[..2], &digits[2..4], "0"),
        6 => (&digits[..2], &digits[2..4], &digits[4..6]),
        _ => return Err(false),
    };
    let (hh, mm, ss): (u64, u64, u64) = (
        hh.parse().unwrap_or(0),
        mm.parse().unwrap_or(0),
        ss.parse().unwrap_or(0),
    );
    if mm > 59 || ss > 60 || hh * 3600 + mm * 60 + ss > 24 * 3600 {
        return Err(true);
    }
    Ok(())
}

/// PG's ValidateDate month/day check (Gregorian leap years).
fn valid_month_day(y: u32, m: u32, d: u32) -> bool {
    if !(1..=12).contains(&m) || d == 0 {
        return false;
    }
    let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let dim = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    d <= dim
}

/// A lone concatenated date field (PG `DecodeNumber`/`DecodeNumberField`):
/// 6 digits is `yymmdd`, 8 is `yyyymmdd` (month/day validated, leap years
/// included); 1–2 digits could only start a date (syntax when it fits a
/// month, otherwise field overflow); 3–5 digits never decode; 7 or ≥ 9
/// digits overflow the field. `Err(true)` = out of range, `Err(false)` =
/// invalid syntax.
fn decode_date_field(digits: &str) -> Result<(), bool> {
    let valid_md = valid_month_day;
    match digits.len() {
        1 | 2 => {
            let v: u32 = digits.parse().unwrap_or(0);
            Err(v > 12)
        }
        3..=5 => Err(false),
        6 => {
            let yy: u32 = digits[..2].parse().unwrap_or(0);
            let y = if yy < 70 { 2000 + yy } else { 1900 + yy };
            let m: u32 = digits[2..4].parse().unwrap_or(0);
            let d: u32 = digits[4..6].parse().unwrap_or(0);
            if valid_md(y, m, d) { Ok(()) } else { Err(true) }
        }
        8 => {
            let y: u32 = digits[..4].parse().unwrap_or(0);
            let m: u32 = digits[4..6].parse().unwrap_or(0);
            let d: u32 = digits[6..8].parse().unwrap_or(0);
            if valid_md(y, m, d) { Ok(()) } else { Err(true) }
        }
        _ => Err(true),
    }
}

// ─── bit strings ────────────────────────────────────────────────────────────

/// Mirrors `bit_in`/`varbit_in` (varbit.c): a leading `b`/`B` selects binary
/// digits, `x`/`X` selects hex digits, anything else is parsed as binary
/// from the first character. Whitespace is *not* trimmed (a space is just an
/// invalid digit). Length-vs-typmod mismatches are a different error owned
/// by the typmod layer and not modeled here.
pub(crate) fn validate_bit(content: &str) -> Result<(), String> {
    let (digits, hex) = match content.as_bytes().first() {
        Some(b'b' | b'B') => (&content[1..], false),
        Some(b'x' | b'X') => (&content[1..], true),
        _ => (content, false),
    };
    for c in digits.chars() {
        let ok = if hex {
            c.is_ascii_hexdigit()
        } else {
            c == '0' || c == '1'
        };
        if !ok {
            return Err(format!(
                "\"{c}\" is not a valid {} digit",
                if hex { "hexadecimal" } else { "binary" }
            ));
        }
    }
    Ok(())
}

// ─── money ──────────────────────────────────────────────────────────────────

/// Mirrors `cash_in` (cash.c) under the C locale's fallback symbols
/// (`$` currency, `,` thousands, `.` decimal): optional whitespace, an
/// optional `(` (negative) or sign, an optional `$` (sign also accepted
/// after it), then digits with free-form `,` separators and at most one
/// decimal point; trailing whitespace / `)` / `$` allowed. At least one
/// digit is required. The range check only fires for magnitudes no int64
/// cent count could hold (≥ 18 integer digits) — kept conservative.
fn validate_money(content: &str) -> Result<(), String> {
    let err = || crate::pgmsg::invalid_input_syntax_for_type("money", content);
    let mut s = content.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if let Some(rest) = s.strip_prefix('(') {
        s = rest;
    } else if let Some(rest) = s.strip_prefix(['+', '-']) {
        s = rest;
    }
    s = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if let Some(rest) = s.strip_prefix('$') {
        s = rest;
        if let Some(rest) = s.strip_prefix(['+', '-']) {
            s = rest;
        }
    }
    let mut int_digits = 0usize;
    let mut any_digit = false;
    let mut seen_dot = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                any_digit = true;
                if !seen_dot {
                    int_digits += 1;
                }
            }
            b',' if !seen_dot => {}
            b'.' if !seen_dot => seen_dot = true,
            _ => break,
        }
        i += 1;
    }
    // The empty string is a valid money input on PG 18 (parses as $0.00),
    // so digits are only required once any non-money character appears.
    if !any_digit && !s[i..].is_empty() {
        return Err(err());
    }
    // Trailing: whitespace, `)`, and a trailing currency symbol are accepted.
    if !s[i..]
        .chars()
        .all(|c| c.is_ascii_whitespace() || c == ')' || c == '$')
    {
        return Err(err());
    }
    if int_digits >= 18 {
        return Err(format!(
            "value \"{content}\" is out of range for type money"
        ));
    }
    Ok(())
}

// ─── network types ──────────────────────────────────────────────────────────

/// One IPv4 dotted-quad. `exact` requires all 4 octets (inet); cidr accepts
/// the abbreviated 1–3 octet forms (`'10/8'::cidr`).
fn valid_ipv4(s: &str, exact: bool) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 4 || (exact && parts.len() != 4) || parts.is_empty() {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.len() <= 3
            && p.chars().all(|c| c.is_ascii_digit())
            && p.parse::<u16>().is_ok_and(|v| v <= 255)
    })
}

/// IPv6 textual form: up to 8 hex groups of 1–4 digits, at most one `::`
/// elision, optional dotted-quad in the last position.
fn valid_ipv6(addr: &str) -> bool {
    let (head, tail, elided) = match addr.find("::") {
        Some(i) => (&addr[..i], &addr[i + 2..], true),
        None => (addr, "", false),
    };
    // A second `::` is malformed.
    if tail.contains("::") {
        return false;
    }
    let side_groups = |s: &str, v4_allowed: bool| -> Option<u32> {
        if s.is_empty() {
            return Some(0);
        }
        let parts: Vec<&str> = s.split(':').collect();
        let mut groups = 0u32;
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty() {
                return None;
            }
            if p.contains('.') {
                if !v4_allowed || i != parts.len() - 1 || !valid_ipv4(p, true) {
                    return None;
                }
                groups += 2;
            } else if p.len() <= 4 && p.chars().all(|c| c.is_ascii_hexdigit()) {
                groups += 1;
            } else {
                return None;
            }
        }
        Some(groups)
    };
    let Some(head_groups) = side_groups(head, !elided) else {
        return false;
    };
    let Some(tail_groups) = side_groups(tail, true) else {
        return false;
    };
    let total = head_groups + tail_groups;
    if elided { total <= 7 } else { total == 8 }
}

/// Mirrors `inet_in` / `cidr_in` (network.c): an IPv4 dotted-quad or an
/// IPv6 address, with an optional `/bits` netmask (≤ 32 / ≤ 128). cidr also
/// accepts the abbreviated IPv4 forms; the cidr "host bits set" check is a
/// different error and not modeled (accepted).
fn validate_inet(content: &str, is_cidr: bool) -> Result<(), String> {
    let name = if is_cidr { "cidr" } else { "inet" };
    let err = || format!("invalid input syntax for type {name}: \"{content}\"");
    let s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    let (addr, mask) = match s.split_once('/') {
        Some((a, m)) => (a, Some(m)),
        None => (s, None),
    };
    let is_v6 = addr.contains(':');
    if let Some(m) = mask {
        let limit = if is_v6 { 128 } else { 32 };
        if m.is_empty()
            || !m.chars().all(|c| c.is_ascii_digit())
            || m.parse::<u32>().is_ok_and(|v| v > limit)
            || m.len() > 3
        {
            return Err(err());
        }
    }
    let ok = if is_v6 {
        valid_ipv6(addr)
    } else {
        valid_ipv4(addr, !is_cidr)
    };
    if ok { Ok(()) } else { Err(err()) }
}

/// Mirrors `macaddr_in` / `macaddr8_in` (mac.c / mac8.c) loosely: hex digits
/// in groups separated by `:`, `-` or `.`; 12 digits total for macaddr, 12
/// or 16 for macaddr8 (6-byte MACs expand via FF:FE). Separator *placement*
/// is not modeled (PG's fixed sscanf formats are stricter) — accept-leaning.
fn validate_macaddr(content: &str, is_mac8: bool) -> Result<(), String> {
    let name = if is_mac8 { "macaddr8" } else { "macaddr" };
    let err = || format!("invalid input syntax for type {name}: \"{content}\"");
    let s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    let mut ndigits = 0usize;
    for c in s.chars() {
        if c.is_ascii_hexdigit() {
            ndigits += 1;
        } else if !matches!(c, ':' | '-' | '.') {
            return Err(err());
        }
    }
    let ok = ndigits == 12 || (is_mac8 && ndigits == 16);
    if ok { Ok(()) } else { Err(err()) }
}

// ─── geometric types ────────────────────────────────────────────────────────

/// Mirrors the geo_ops.c input functions loosely: tokenize the value into
/// float coordinates (sign / digits / `.` / exponent; `nan` and
/// `inf`/`infinity` are valid coordinates) amid the delimiter set
/// `, ( ) [ ] < > { }` and whitespace, then check the coordinate *count*
/// each shape requires. Delimiter placement is not modeled (accept-leaning):
/// `'(1,2]'::point` passes here even though PG rejects it.
fn validate_geometric(content: &str, name: &str) -> Result<(), String> {
    let err = || format!("invalid input syntax for type {name}: \"{content}\"");
    let mut nums = 0usize;
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_whitespace()
            || matches!(c, ',' | '(' | ')' | '[' | ']' | '<' | '>' | '{' | '}')
        {
            i += 1;
            continue;
        }
        // A coordinate: optional sign, then nan/inf[inity] or a decimal
        // float with optional exponent.
        let start = i;
        if matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        let rest = &content[i..];
        let lower = rest
            .get(..8.min(rest.len()))
            .unwrap_or("")
            .to_ascii_lowercase();
        if lower.starts_with("infinity") {
            i += 8;
            nums += 1;
            continue;
        }
        if lower.starts_with("inf") {
            i += 3;
            nums += 1;
            continue;
        }
        if lower.starts_with("nan") {
            i += 3;
            nums += 1;
            continue;
        }
        let mut any = false;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            any = true;
        }
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
                any = true;
            }
        }
        if any && i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
            let mut j = i + 1;
            if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
                j += 1;
            }
            let mut exp_digit = false;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
                exp_digit = true;
            }
            if exp_digit {
                i = j;
            }
        }
        if !any || i == start {
            return Err(err());
        }
        nums += 1;
    }
    let count_ok = match name {
        "point" => nums == 2,
        "lseg" | "box" => nums == 4,
        "circle" => nums == 3,
        // `{A,B,C}` (3) or two points (4).
        "line" => nums == 3 || nums == 4,
        // One or more points.
        "path" | "polygon" => nums >= 2 && nums.is_multiple_of(2),
        _ => true,
    };
    if count_ok { Ok(()) } else { Err(err()) }
}

// ─── system identifier types ────────────────────────────────────────────────

/// Mirrors `tidin` (tid.c): `(block,offset)` with two unsigned decimal
/// numbers. Surrounding whitespace tolerated (accept-leaning).
fn validate_tid(content: &str) -> Result<(), String> {
    let err = || format!("invalid input syntax for type tid: \"{content}\"");
    let s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    let inner = s
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .ok_or_else(err)?;
    let (block, offset) = inner.split_once(',').ok_or_else(err)?;
    for part in [block, offset] {
        let p = part.trim_matches(|c: char| c.is_ascii_whitespace());
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return Err(err());
        }
    }
    Ok(())
}

/// Mirrors `pg_lsn_in`: `XXX/XXX` with 1–8 hex digits on each side.
fn validate_pg_lsn(content: &str) -> Result<(), String> {
    let err = || format!("invalid input syntax for type pg_lsn: \"{content}\"");
    let s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    let (hi, lo) = s.split_once('/').ok_or_else(err)?;
    for part in [hi, lo] {
        if part.is_empty() || part.len() > 8 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(err());
        }
    }
    Ok(())
}

/// `xid` / `xid8` / `cid` parse via strtoul-style rules on PG 18: decimal
/// digits or a `0x` hex prefix (bare hex like `ff` is rejected), optional
/// sign (the parse wraps like strtoul). No range check.
fn validate_xid(content: &str, name: &str) -> Result<(), String> {
    let s = content.trim_matches(|c: char| c.is_ascii_whitespace());
    let mut digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    let mut radix = 10;
    if let Some(rest) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        digits = rest;
        radix = 16;
    }
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(format!(
            "invalid input syntax for type {name}: \"{content}\""
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Pure-string validators are testable without a catalog; the
    // catalog-dependent paths (enum, array, range, reg*) are covered by the
    // analyzer's query tests.
    use super::*;

    #[test]
    fn bool_inputs() {
        for ok in ["t", "tr", "TRUE", " yes ", "of", "off", "1", "0", "on", "n"] {
            assert!(validate_bool(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "o", "10", "x", "tru e", "onn"] {
            assert!(validate_bool(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn int_inputs() {
        let v = |s| validate_int(s, i32::MIN as i128, i32::MAX as i128, "integer");
        for ok in [
            " 42 ",
            "+42",
            "-2147483648",
            "0x1F",
            "0o17",
            "0b101",
            "1_000",
            "0x1_F",
        ] {
            assert!(v(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "- 42", "42abc", "0x", "1__0", "_1", "1_", "0x_1"] {
            assert_eq!(
                v(bad).unwrap_err(),
                format!("invalid input syntax for type integer: \"{bad}\""),
            );
        }
        assert_eq!(
            v("2147483648").unwrap_err(),
            "value \"2147483648\" is out of range for type integer"
        );
        assert_eq!(
            v("0xFFFFFFFF").unwrap_err(),
            "value \"0xFFFFFFFF\" is out of range for type integer"
        );
    }

    #[test]
    fn float_inputs() {
        let v = |s| validate_float(s, "double precision");
        for ok in [
            "1",
            ".5",
            "5.",
            "1e3",
            "5.e-3",
            "inf",
            "-Infinity",
            "NaN",
            "0x1F",
            "0x1.8p3",
        ] {
            assert!(v(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "1e", "1_000", "x", "1.2.3", "0x", "1e+"] {
            assert!(v(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn numeric_inputs() {
        for ok in [
            "1_000.5_0",
            "0x1F",
            "NaN",
            " inf ",
            "-Infinity",
            "1e10",
            ".5",
            "5.",
            "1.5e+3",
        ] {
            assert!(validate_numeric(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "1e", "1.2.3", "hello", "0x", "5..", "._5"] {
            assert!(validate_numeric(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn uuid_inputs() {
        for ok in [
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "A0EEBC999C0B4EF8BB6D6BB9BD380A11",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
        ] {
            assert!(validate_uuid(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in [
            "",
            "xyz",
            "a0-eebc999c0b4ef8bb6d6bb9bd380a11",
            " a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11 ",
            "a0eebc999c0b4ef8bb6d6bb9bd380a111",
        ] {
            assert!(validate_uuid(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn json_inputs() {
        for ok in [
            "{}",
            " {\"a\": [1, -0.5e+3, true, null, \"\\u00ff\"]} ",
            "1.5e3",
            "\"x\"",
            "[1 , 2]",
            "-0",
        ] {
            assert!(validate_json(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in [
            "",
            "  ",
            "01",
            "nullx",
            "[1,]",
            "{\"a\"}",
            "\"\\u00zz\"",
            "{1:2}",
            "'x'",
        ] {
            assert!(validate_json(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn bit_inputs() {
        for ok in ["101", "x1F", "b101", "", "X0aF"] {
            assert!(validate_bit(ok).is_ok(), "{ok:?} should be valid");
        }
        assert_eq!(
            validate_bit("102").unwrap_err(),
            "\"2\" is not a valid binary digit"
        );
        assert_eq!(
            validate_bit("xFG").unwrap_err(),
            "\"G\" is not a valid hexadecimal digit"
        );
        assert!(validate_bit(" 42 ").is_err());
        assert!(validate_bit("NaN").is_err());
    }

    #[test]
    fn money_inputs() {
        for ok in [
            "123",
            "$123.45",
            "-$1,000.00",
            "($123)",
            "$-123",
            "  12  ",
            "",
        ] {
            assert!(validate_money(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["hello", "(1,2]", "1.2.3"] {
            assert!(validate_money(bad).is_err(), "{bad:?} should be invalid");
        }
        assert_eq!(
            validate_money("9999999999999999999999").unwrap_err(),
            "value \"9999999999999999999999\" is out of range for type money"
        );
    }

    #[test]
    fn inet_inputs() {
        for ok in [
            "192.168.0.1",
            "192.168.0.1/24",
            "::1",
            "fe80::1/64",
            "::ffff:192.168.0.1",
            "1:2:3:4:5:6:7:8",
        ] {
            assert!(validate_inet(ok, false).is_ok(), "{ok:?} should be valid");
        }
        for bad in [
            "42",
            "192.168",
            "256.1.1.1",
            "192.168.0.1/33",
            "hello",
            "1:2:3:4:5:6:7:8:9",
            "1::2::3",
        ] {
            assert!(
                validate_inet(bad, false).is_err(),
                "{bad:?} should be invalid"
            );
        }
        for ok in ["10/8", "10.1/16", "192.168.0.0/24"] {
            assert!(
                validate_inet(ok, true).is_ok(),
                "{ok:?} should be valid cidr"
            );
        }
        assert!(validate_inet("x/8", true).is_err());
    }

    #[test]
    fn macaddr_inputs() {
        for ok in [
            "aa:bb:cc:dd:ee:ff",
            "aa-bb-cc-dd-ee-ff",
            "aabb.ccdd.eeff",
            "aabbccddeeff",
        ] {
            assert!(
                validate_macaddr(ok, false).is_ok(),
                "{ok:?} should be valid"
            );
        }
        for bad in ["aa:bb:cc:dd:ee", " 42 ", "hello", "zz:bb:cc:dd:ee:ff"] {
            assert!(
                validate_macaddr(bad, false).is_err(),
                "{bad:?} should be invalid"
            );
        }
        assert!(validate_macaddr("aa:bb:cc:dd:ee:ff:00:11", true).is_ok());
        assert!(validate_macaddr("aa:bb:cc:dd:ee:ff", true).is_ok());
    }

    #[test]
    fn geometric_inputs() {
        for (ok, ty) in [
            ("(1,2)", "point"),
            ("1,2", "point"),
            ("(NaN,NaN)", "point"),
            ("(1.5e3,-2)", "point"),
            ("((0,0),(1,1))", "box"),
            ("[(0,0),(1,1)]", "lseg"),
            ("<(0,0),5>", "circle"),
            ("{1,2,3}", "line"),
            ("((0,0),(1,1),(2,0))", "polygon"),
            ("(1,2)", "path"),
        ] {
            assert!(
                validate_geometric(ok, ty).is_ok(),
                "{ok:?}::{ty} should be valid"
            );
        }
        for (bad, ty) in [
            ("3.14", "point"),
            ("hello", "point"),
            (" 42 ", "box"),
            ("(1,2)", "lseg"),
            ("(1,2)", "circle"),
            ("{1,2}", "line"),
            ("(1,2,3)", "path"),
        ] {
            assert!(
                validate_geometric(bad, ty).is_err(),
                "{bad:?}::{ty} should be invalid"
            );
        }
    }

    #[test]
    fn system_id_inputs() {
        assert!(validate_tid("(0,1)").is_ok());
        assert!(validate_tid("(0)").is_err());
        assert!(validate_tid("42").is_err());
        assert!(validate_pg_lsn("0/0").is_ok());
        assert!(validate_pg_lsn("AB/CDEF1234").is_ok());
        assert!(validate_pg_lsn("0").is_err());
        assert!(validate_pg_lsn("X/Y").is_err());
        assert!(validate_xid("42", "xid").is_ok());
        assert!(validate_xid("0x10", "xid").is_ok());
        assert!(validate_xid("ff", "xid").is_err());
    }

    #[test]
    fn oid_inputs() {
        for ok in [" 42 ", "-1", "0x10", "+7"] {
            assert!(validate_oid(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in ["", "1_0", "42x", "0x", "hello"] {
            assert!(validate_oid(bad).is_err(), "{bad:?} should be invalid");
        }
    }
}
