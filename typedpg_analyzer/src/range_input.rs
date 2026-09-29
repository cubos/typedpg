//! Ports of PostgreSQL 18's range and multirange input functions
//! (rangetypes.c `range_in` / `range_parse` / `range_parse_bound` /
//! `range_serialize` and the built-in `*_canonical` functions;
//! multirangetypes.c `multirange_in`), used by [`crate::literal_input`].
//!
//! Bound strings are validated with the subtype's own input rules (through
//! [`crate::literal_input::validate`]). The `lower <= upper` check and the
//! integer canonicalization overflow are modelled for the built-in range
//! types, whose subtype ordering is the default btree order and computable
//! exactly here — and only when both bounds are in a shape we can order
//! without ambiguity (see [`BoundKey`]); anything else skips the ordering
//! check (accepts).

use crate::oid::PgTypeOid;
use crate::pg_catalog::PgCatalog;
use std::cmp::Ordering;

/// C `isspace` in the C locale.
fn c_isspace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// A parsed range literal (`range_parse`).
struct RangeParts {
    empty: bool,
    lower: Option<String>,
    upper: Option<String>,
    lower_inc: bool,
    upper_inc: bool,
}

/// `range_parse` + `range_parse_bound`: the literal's structure, with each
/// bound de-quoted (`""` inside quotes is a quote, `\x` is `x`). `None`
/// bounds are infinite.
fn parse_range(content: &str) -> Result<RangeParts, String> {
    let malformed = || format!("malformed range literal: \"{content}\"");
    let b = content.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut p = 0;
    while c_isspace(at(p)) {
        p += 1;
    }
    if b.len() >= p + 5 && b[p..p + 5].eq_ignore_ascii_case(b"empty") {
        p += 5;
        while c_isspace(at(p)) {
            p += 1;
        }
        if p != b.len() {
            return Err(malformed());
        }
        return Ok(RangeParts {
            empty: true,
            lower: None,
            upper: None,
            lower_inc: false,
            upper_inc: false,
        });
    }
    let lower_inc = match at(p) {
        b'[' => true,
        b'(' => false,
        _ => return Err(malformed()),
    };
    p += 1;
    let parse_bound = |p: &mut usize| -> Result<Option<String>, String> {
        if matches!(at(*p), b',' | b')' | b']') {
            return Ok(None);
        }
        let mut buf = Vec::new();
        let mut inquote = false;
        while inquote || !matches!(at(*p), b',' | b')' | b']') {
            let ch = at(*p);
            *p += 1;
            match ch {
                0 => return Err(malformed()),
                b'\\' => {
                    if at(*p) == 0 {
                        return Err(malformed());
                    }
                    buf.push(at(*p));
                    *p += 1;
                }
                b'"' => {
                    if !inquote {
                        inquote = true;
                    } else if at(*p) == b'"' {
                        buf.push(b'"');
                        *p += 1;
                    } else {
                        inquote = false;
                    }
                }
                c => buf.push(c),
            }
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    };
    let lower = parse_bound(&mut p)?;
    if at(p) != b',' {
        return Err(malformed());
    }
    p += 1;
    let upper = parse_bound(&mut p)?;
    let upper_inc = match at(p) {
        b']' => true,
        b')' => false,
        _ => return Err(malformed()),
    };
    p += 1;
    while c_isspace(at(p)) {
        p += 1;
    }
    if p != b.len() {
        return Err(malformed());
    }
    Ok(RangeParts {
        empty: false,
        lower,
        upper,
        lower_inc,
        upper_inc,
    })
}

/// Mirrors `range_in`: parse, validate each bound with the subtype's input
/// rules (lower first), then `range_serialize`'s ordering check and the
/// built-in canonical function's overflow check.
pub(crate) fn validate_range(
    content: &str,
    range_oid: PgTypeOid,
    snapshot: &PgCatalog,
) -> Result<(), String> {
    let parts = parse_range(content)?;
    let Some(subtype) = snapshot.range_subtype(range_oid) else {
        return Ok(());
    };
    for bound in [&parts.lower, &parts.upper].into_iter().flatten() {
        crate::literal_input::validate(bound, subtype, snapshot)?;
    }
    if parts.empty {
        return Ok(());
    }
    let (Some(lower), Some(upper)) = (&parts.lower, &parts.upper) else {
        // An infinite bound always orders below / above the other one.
        return check_canonical(&parts, range_oid, snapshot);
    };
    let Some(kind) = builtin_range_kind(range_oid, snapshot) else {
        return Ok(());
    };
    let (Some(lo), Some(hi)) = (BoundKey::parse(lower, kind), BoundKey::parse(upper, kind)) else {
        return Ok(());
    };
    match lo.cmp(&hi) {
        Ordering::Greater => {
            Err("range lower bound must be less than or equal to range upper bound".to_string())
        }
        // Equal bounds that are not both inclusive make an empty range,
        // which is never canonicalized.
        Ordering::Equal if !(parts.lower_inc && parts.upper_inc) => Ok(()),
        _ => check_canonical(&parts, range_oid, snapshot),
    }
}

/// `int4range_canonical` / `int8range_canonical`: an exclusive lower or
/// inclusive upper bound at the type's maximum can't be shifted by one.
/// (`daterange_canonical` can only overflow past year 5874897 — out of the
/// shapes [`BoundKey`] parses.)
fn check_canonical(
    parts: &RangeParts,
    range_oid: PgTypeOid,
    snapshot: &PgCatalog,
) -> Result<(), String> {
    let (max, msg) = match builtin_range_kind(range_oid, snapshot) {
        Some(Kind::Int4) => (i128::from(i32::MAX), "integer out of range"),
        Some(Kind::Int8) => (i128::from(i64::MAX), "bigint out of range"),
        _ => return Ok(()),
    };
    let at_max = |b: &Option<String>| {
        b.as_deref()
            .and_then(crate::literal_input::parse_pg_integer)
            .is_some_and(|v| v == max)
    };
    if (!parts.lower_inc && at_max(&parts.lower)) || (parts.upper_inc && at_max(&parts.upper)) {
        return Err(msg.to_string());
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int4,
    Int8,
    Numeric,
    Date,
    Timestamp,
    Timestamptz,
}

/// The built-in range types (default subtype opclass, known canonical
/// function). User-defined ranges may carry a custom `subtype_opclass` the
/// catalog doesn't record, so their ordering is never checked.
fn builtin_range_kind(range_oid: PgTypeOid, snapshot: &PgCatalog) -> Option<Kind> {
    let t = snapshot.get_type(range_oid)?;
    if snapshot.namespace_name(t.typnamespace) != Some("pg_catalog") {
        return None;
    }
    Some(match t.typname.as_str() {
        "int4range" => Kind::Int4,
        "int8range" => Kind::Int8,
        "numrange" => Kind::Numeric,
        "daterange" => Kind::Date,
        "tsrange" => Kind::Timestamp,
        "tstzrange" => Kind::Timestamptz,
        _ => return None,
    })
}

/// A bound value reduced to a totally ordered key matching the subtype's
/// btree order. `parse` returns `None` for any shape it can't order with
/// certainty.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum BoundKey {
    NegInf,
    /// Integers, and dates/timestamps as microseconds since 0001-01-01.
    Int(i128),
    /// A finite numeric: see [`Decimal`].
    Dec(Decimal),
    PosInf,
    /// numeric `NaN` sorts above everything, including `Infinity`.
    NaN,
}

impl BoundKey {
    fn parse(s: &str, kind: Kind) -> Option<BoundKey> {
        match kind {
            Kind::Int4 | Kind::Int8 => crate::literal_input::parse_pg_integer(s).map(BoundKey::Int),
            Kind::Numeric => parse_numeric_key(s),
            Kind::Date | Kind::Timestamp | Kind::Timestamptz => parse_datetime_key(s, kind),
        }
    }
}

/// A finite numeric value, normalized so the derived ordering is numeric
/// order: `(sign class, magnitude)`, where the magnitude is `0.D × 10^exp`
/// with `D` free of leading/trailing zeros — flipped for negatives.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Decimal {
    Neg(std::cmp::Reverse<(i64, Vec<u8>)>),
    Zero,
    Pos((i64, Vec<u8>)),
}

/// `numeric_in`'s value, for ordering. Radix-prefixed integers beyond
/// `u128` and exponents beyond ±1000 (near numeric's own overflow limits,
/// whose errors would take precedence) are not ordered.
fn parse_numeric_key(s: &str) -> Option<BoundKey> {
    let t = s.trim_matches(|c: char| c.is_ascii() && c_isspace(c as u8));
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let lower = body.to_ascii_lowercase();
    if lower == "nan" {
        return (!t.starts_with(['+', '-'])).then_some(BoundKey::NaN);
    }
    if lower == "inf" || lower == "infinity" {
        return Some(if neg {
            BoundKey::NegInf
        } else {
            BoundKey::PosInf
        });
    }
    let bytes = body.as_bytes();
    let (digits, exp): (Vec<u8>, i64) =
        if bytes.len() >= 2 && bytes[0] == b'0' && matches!(bytes[1] | 0x20, b'x' | b'o' | b'b') {
            let radix = match bytes[1] | 0x20 {
                b'x' => 16,
                b'o' => 8,
                _ => 2,
            };
            let v = u128::from_str_radix(&body[2..].replace('_', ""), radix).ok()?;
            let d = v.to_string().into_bytes();
            let e = d.len() as i64;
            (d, e)
        } else {
            let clean = body.replace('_', "");
            let (mantissa, exponent) = match clean.find(['e', 'E']) {
                Some(i) => (&clean[..i], clean[i + 1..].parse::<i64>().ok()?),
                None => (clean.as_str(), 0),
            };
            if exponent.abs() > 1000 {
                return None;
            }
            let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
            if !int_part
                .bytes()
                .chain(frac_part.bytes())
                .all(|c| c.is_ascii_digit())
            {
                return None;
            }
            let d: Vec<u8> = int_part.bytes().chain(frac_part.bytes()).collect();
            (d, int_part.len() as i64 + exponent)
        };
    let lead = digits.iter().position(|&c| c != b'0');
    let Some(lead) = lead else {
        return Some(BoundKey::Dec(Decimal::Zero));
    };
    let mut d = digits[lead..].to_vec();
    while d.last() == Some(&b'0') {
        d.pop();
    }
    let mag = (exp - lead as i64, d);
    Some(BoundKey::Dec(if neg {
        Decimal::Neg(std::cmp::Reverse(mag))
    } else {
        Decimal::Pos(mag)
    }))
}

/// Days from 0001-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe
}

/// Date / timestamp / timestamptz values in the unambiguous ISO shapes:
/// `[+-]infinity`, `YYYY-MM-DD` (4-digit year), optionally followed (not
/// for dates) by a space or `T` and `HH:MM[:SS[.f{1,6}]]`. timestamptz
/// additionally requires an explicit `±HH[:MM]` offset — a zone-less value
/// depends on the session's TimeZone. Everything else (DateStyle-dependent
/// orders, BC, special words, zone names, …) is not ordered.
fn parse_datetime_key(s: &str, kind: Kind) -> Option<BoundKey> {
    let t = s
        .trim_matches(|c: char| c.is_ascii() && c_isspace(c as u8))
        .to_ascii_lowercase();
    match t.as_str() {
        "infinity" | "+infinity" => return Some(BoundKey::PosInf),
        "-infinity" => return Some(BoundKey::NegInf),
        _ => {}
    }
    let b = t.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = b.get(r)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let dim = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return None,
    };
    if y == 0 || d == 0 || d > dim {
        return None;
    }
    let day_us = i128::from(days_from_civil(y, m, d)) * 86_400_000_000;
    let rest = &b[10..];
    if rest.is_empty() {
        return match kind {
            Kind::Date | Kind::Timestamp => Some(BoundKey::Int(day_us)),
            _ => None,
        };
    }
    if kind == Kind::Date || !matches!(rest[0], b' ' | b't') {
        return None;
    }
    let rest = &rest[1..];
    // HH:MM[:SS[.frac]]
    let two = |r: &[u8], i: usize| -> Option<i64> {
        let p = r.get(i..i + 2)?;
        p.iter()
            .all(u8::is_ascii_digit)
            .then(|| i64::from(p[0] - b'0') * 10 + i64::from(p[1] - b'0'))
    };
    let hh = two(rest, 0)?;
    if rest.get(2) != Some(&b':') {
        return None;
    }
    let mi = two(rest, 3)?;
    let mut i = 5;
    let mut ss = 0;
    let mut us = 0i64;
    if rest.get(i) == Some(&b':') {
        ss = two(rest, i + 1)?;
        i += 3;
        if rest.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            while rest.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            let frac = &rest[start..i];
            if frac.is_empty() || frac.len() > 6 {
                return None;
            }
            let mut f = 0i64;
            for k in 0..6 {
                f = f * 10 + frac.get(k).map_or(0, |c| i64::from(c - b'0'));
            }
            us = f;
        }
    }
    let time_ok = (hh < 24 && mi < 60 && ss < 60) || (hh == 24 && mi == 0 && ss == 0 && us == 0);
    if !time_ok {
        return None;
    }
    let mut value = day_us + i128::from(((hh * 60 + mi) * 60 + ss) * 1_000_000 + us);
    let tail = &rest[i..];
    match kind {
        Kind::Timestamp => tail.is_empty().then_some(BoundKey::Int(value)),
        _ => {
            // ±HH or ±HH:MM, within PG's ±15:59 zone limit.
            let sign = match tail.first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let oh = two(tail, 1)?;
            let om = match tail.len() {
                3 => 0,
                6 if tail[3] == b':' => two(tail, 4)?,
                _ => return None,
            };
            if oh > 15 || om > 59 {
                return None;
            }
            value -= i128::from(sign * (oh * 60 + om) * 60 * 1_000_000);
            Some(BoundKey::Int(value))
        }
    }
}

/// Mirrors `multirange_in`'s scanner: `{`, then comma-separated ranges
/// (each validated as a range literal of `range_oid`, in order) or
/// `empty`, then `}` and trailing whitespace. Whitespace is skipped in
/// every state (even inside quotes, for the state machine only).
pub(crate) fn validate_multirange(
    content: &str,
    range_oid: PgTypeOid,
    snapshot: &PgCatalog,
) -> Result<(), String> {
    #[derive(PartialEq)]
    enum State {
        BeforeRange,
        InRange,
        InRangeEscaped,
        InRangeQuoted,
        InRangeQuotedEscaped,
        AfterRange,
        Finished,
    }
    let malformed = || format!("malformed multirange literal: \"{content}\"");
    let b = content.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut p = 0;
    while c_isspace(at(p)) {
        p += 1;
    }
    if at(p) != b'{' {
        return Err(malformed());
    }
    p += 1;
    let mut state = State::BeforeRange;
    let mut ranges_seen = 0;
    let mut range_start = 0;
    while state != State::Finished {
        let ch = at(p);
        if ch == 0 {
            return Err(malformed());
        }
        if c_isspace(ch) {
            p += 1;
            continue;
        }
        match state {
            State::BeforeRange => {
                if ch == b'[' || ch == b'(' {
                    range_start = p;
                    state = State::InRange;
                } else if ch == b'}' && ranges_seen == 0 {
                    state = State::Finished;
                } else if b.len() >= p + 5 && b[p..p + 5].eq_ignore_ascii_case(b"empty") {
                    ranges_seen += 1;
                    p += 4;
                    state = State::AfterRange;
                } else {
                    return Err(malformed());
                }
            }
            State::InRange => {
                if ch == b']' || ch == b')' {
                    ranges_seen += 1;
                    let range_str = String::from_utf8_lossy(&b[range_start..=p]);
                    validate_range(&range_str, range_oid, snapshot)?;
                    state = State::AfterRange;
                } else if ch == b'"' {
                    state = State::InRangeQuoted;
                } else if ch == b'\\' {
                    state = State::InRangeEscaped;
                }
            }
            State::InRangeEscaped => state = State::InRange,
            State::InRangeQuoted => {
                if ch == b'"' {
                    if at(p + 1) == b'"' {
                        p += 1;
                    } else {
                        state = State::InRange;
                    }
                } else if ch == b'\\' {
                    state = State::InRangeQuotedEscaped;
                }
            }
            State::InRangeQuotedEscaped => state = State::InRangeQuoted,
            State::AfterRange => {
                if ch == b',' {
                    state = State::BeforeRange;
                } else if ch == b'}' {
                    state = State::Finished;
                } else {
                    return Err(malformed());
                }
            }
            State::Finished => unreachable!(),
        }
        p += 1;
    }
    while c_isspace(at(p)) {
        p += 1;
    }
    if p != b.len() {
        return Err(malformed());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_structure() {
        for ok in [
            " empty ",
            "EMPTY",
            "(,)",
            "[1,2)",
            "[ 1 , 2 )",
            "[\"1\",\"2\")",
            "[\\1,2)",
        ] {
            assert!(parse_range(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "", "emptyx", "[1,2)x", "[1,2,3)", "[1", "[a,b", "[1\\", "1,2",
        ] {
            assert_eq!(
                parse_range(bad).err().unwrap(),
                format!("malformed range literal: \"{bad}\""),
            );
        }
        let p = parse_range("[\"a\"\"b\",\\,x)").unwrap();
        assert_eq!(p.lower.as_deref(), Some("a\"b"));
        assert_eq!(p.upper.as_deref(), Some(",x"));
    }

    #[test]
    fn numeric_keys_order_numerically() {
        let k = |s| parse_numeric_key(s).unwrap();
        assert!(k("1.5") > k("1.4"));
        assert!(k("1.5") == k("1.50"));
        assert!(k("-0") == k("0.000"));
        assert!(k("1e3") > k("999"));
        assert!(k("-2") < k("-1.5"));
        assert!(k("0x10") > k("15"));
        assert!(k("1_000") > k("999"));
        assert!(k("NaN") > k("infinity"));
        assert!(k("-inf") < k("-1e1000"));
        assert!(k("0.001") < k("0.01"));
        assert!(parse_numeric_key("1e2000").is_none());
    }

    #[test]
    fn datetime_keys() {
        let k = |s, kind| parse_datetime_key(s, kind);
        assert!(k("2024-01-02", Kind::Date) > k("2024-01-01", Kind::Date));
        assert!(k("infinity", Kind::Date) > k("9999-12-31", Kind::Date));
        assert!(
            k("2024-01-01 24:00", Kind::Timestamp) == k("2024-01-02 00:00:00", Kind::Timestamp)
        );
        assert!(
            k("2024-01-01 10:00+02", Kind::Timestamptz)
                < k("2024-01-01 09:00+00", Kind::Timestamptz)
        );
        assert!(k("2024-01-01 10:00", Kind::Timestamptz).is_none());
        assert!(k("2024-02-30", Kind::Date).is_none());
        assert!(k("01/02/2024", Kind::Date).is_none());
        assert!(k("2024-01-01 10:00:00.1234567", Kind::Timestamp).is_none());
    }
}
