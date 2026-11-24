//! The output functions of the built-in types, for the constants a stored
//! expression holds: PG keeps a constant as its value, and `get_const_expr`
//! prints it through the type's output function (`float8out`'s shortest
//! digits, `numeric_out`'s display scale, `date_out` in the session's
//! DateStyle, `jsonb_out`'s normalized object…), while the analyzer keeps
//! the literal as written. [`output`] is what that function prints for
//! the literal, where it is known here.

use crate::ddl::datetime_output::{self as dt, Settings, Style};
use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, TypType};

/// What the output function of type `t` prints for the value literal `v`
/// reads as (its input function, then its output function, in the
/// session's DateStyle, IntervalStyle and TimeZone); `None` where that is
/// not known here.
pub(crate) fn output(interp: &PgCatalog, t: PgTypeOid, v: &str) -> Option<String> {
    text_of(interp, t, v, Style::Session)
}

/// The text of the value literal `v` of type `t` (read under the session's
/// settings) that reads as the same value under every setting — the text
/// a stored literal is kept as — when the type's constants depend on the
/// settings (the datetime family, and arrays, ranges and domains of it).
/// `None` for another type, or where the value isn't known here.
pub(crate) fn canonical(interp: &PgCatalog, t: PgTypeOid, v: &str) -> Option<String> {
    depends_on_settings(interp, t)
        .then(|| text_of(interp, t, v, Style::Canonical))
        .flatten()
}

/// Whether the constants of type `t` are read or printed under the
/// session's settings: a datetime type, or an array, range or domain of
/// one.
pub(crate) fn depends_on_settings(interp: &PgCatalog, t: PgTypeOid) -> bool {
    let base = interp.unwrap_domain(t);
    let Some(ty) = interp.get_type(base) else {
        return false;
    };
    if let Some(elem) = ty.typelem
        && interp.array_type_of(elem) == Some(base)
        && ty.typlen == -1
    {
        return depends_on_settings(interp, elem);
    }
    interp.namespace_name(ty.typnamespace) == Some("pg_catalog")
        && (dt::is_settings_dependent(&ty.typname)
            || matches!(ty.typname.as_str(), "daterange" | "tsrange" | "tstzrange"))
}

/// [`output`] / [`canonical`].
fn text_of(interp: &PgCatalog, t: PgTypeOid, v: &str, style: Style) -> Option<String> {
    let base = interp.unwrap_domain(t);
    let ty = interp.get_type(base)?;
    if ty.typtype == TypType::Enum {
        return Some(v.to_owned());
    }
    if let Some(elem) = ty.typelem
        && interp.array_type_of(elem) == Some(base)
        && ty.typlen == -1
    {
        return array_out(interp, elem, v, style);
    }
    if interp.namespace_name(ty.typnamespace) != Some("pg_catalog") {
        return None;
    }
    if dt::is_settings_dependent(&ty.typname) {
        return dt::output(&ty.typname, v, &Settings::of(interp), style);
    }
    match ty.typname.as_str() {
        "text" | "varchar" | "bpchar" | "unknown" | "json" => Some(v.to_owned()),
        // namein: at most NAMEDATALEN - 1 bytes, cut at a character.
        "name" => {
            let mut end = v.len().min(63);
            while !v.is_char_boundary(end) {
                end -= 1;
            }
            Some(v[..end].to_owned())
        }
        // charin: the first byte (an ASCII one prints as itself).
        "char" => match v.as_bytes().first() {
            None => Some(String::new()),
            Some(b) if b.is_ascii() && *b != b'\\' => Some((*b as char).to_string()),
            _ => None,
        },
        "int2" | "int4" | "int8" => {
            crate::literal_input::parse_pg_integer(v).map(|i| i.to_string())
        }
        "oid" => crate::literal_input::parse_pg_integer(v)
            .filter(|i| (0..=i128::from(u32::MAX)).contains(i))
            .map(|i| i.to_string()),
        "bool" => crate::ddl::reloptions::parse_bool(v.trim())
            .map(|b| if b { "t" } else { "f" }.to_owned()),
        "float4" => float_out(v, true),
        "float8" => float_out(v, false),
        "numeric" => numeric_out(v),
        "bytea" => bytea_out(v),
        "uuid" => uuid_out(v),
        "inet" => inet_out(v, false),
        "cidr" => inet_out(v, true),
        "macaddr" => macaddr_out(v, false),
        "macaddr8" => macaddr_out(v, true),
        "bit" | "varbit" => bit_out(v),
        "money" => money_out(v),
        "pg_lsn" => {
            let (hi, lo) = v.trim().split_once('/')?;
            let hi = u32::from_str_radix(hi, 16).ok()?;
            let lo = u32::from_str_radix(lo, 16).ok()?;
            Some(format!("{hi:X}/{lo:08X}"))
        }
        "jsonb" => {
            let mut p = JsonParser {
                b: v.as_bytes(),
                p: 0,
            };
            let value = p.value()?;
            p.ws();
            if p.p != p.b.len() {
                return None;
            }
            let mut out = String::new();
            value.write(&mut out);
            Some(out)
        }
        "point" => {
            let [x, y] = floats::<2>(v)?;
            Some(point(x, y))
        }
        "lseg" => {
            let [x1, y1, x2, y2] = floats::<4>(v)?;
            Some(format!("[{},{}]", point(x1, y1), point(x2, y2)))
        }
        // box_in: the corners reordered, the upper right first.
        "box" => {
            let [x1, y1, x2, y2] = floats::<4>(v)?;
            Some(format!(
                "{},{}",
                point(x1.max(x2), y1.max(y2)),
                point(x1.min(x2), y1.min(y2))
            ))
        }
        "line" if v.trim_start().starts_with('{') => {
            let [a, b, c] = floats::<3>(v)?;
            Some(format!("{{{},{},{}}}", f8(a), f8(b), f8(c)))
        }
        "circle" => {
            let [x, y, r] = floats::<3>(v)?;
            Some(format!("<{},{}>", point(x, y), f8(r)))
        }
        "path" | "polygon" => {
            let all = all_floats(v)?;
            if all.is_empty() || all.len() % 2 != 0 {
                return None;
            }
            let points: Vec<String> = all.chunks(2).map(|p| point(p[0], p[1])).collect();
            let open = ty.typname == "path" && v.trim_start().starts_with('[');
            Some(if open {
                format!("[{}]", points.join(","))
            } else {
                format!("({})", points.join(","))
            })
        }
        "int4range" | "int8range" | "numrange" | "daterange" | "tsrange" | "tstzrange" => {
            range_out(interp, &ty.typname, v, style)
        }
        // xml_out prints the text kept, but a declaration is rewritten.
        "xml" if !v.trim_start().starts_with("<?xml") => Some(v.to_owned()),
        _ => None,
    }
}

/// `float4out` / `float8out` with the default `extra_float_digits` (1):
/// the shortest digits that read back as the value (Ryu), in fixed
/// notation for decimal exponents from -4 to 5 (`float4`) or 14
/// (`float8`), else as `d.ddde±XX`.
fn float_out(v: &str, single: bool) -> Option<String> {
    let s = v.trim_matches(|c: char| c.is_ascii_whitespace());
    // strtod's forms the Rust parser doesn't read (hexadecimal) give up.
    if s.contains(['x', 'X', '_']) {
        return None;
    }
    if single {
        let x: f32 = s.parse().ok()?;
        Some(shortest(f64::from(x), format!("{:e}", x.abs()), 6))
    } else {
        let x: f64 = s.parse().ok()?;
        Some(shortest(x, format!("{:e}", x.abs()), 15))
    }
}

/// [`float_out`] of a `float8`.
fn f8(x: f64) -> String {
    shortest(x, format!("{:e}", x.abs()), 15)
}

/// The layout of `to_chars` (d2s.c): `x`'s shortest digits as Rust's `{:e}`
/// writes them (`sci`), fixed below exponent `limit`.
fn shortest(x: f64, sci: String, limit: i32) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0");
    }
    let Some((mantissa, exp)) = sci.split_once('e') else {
        return sci;
    };
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    if (-4..limit).contains(&exp) {
        if exp < 0 {
            let zeros = "0".repeat((-exp - 1) as usize);
            format!("{sign}0.{zeros}{digits}")
        } else {
            let int_len = (exp + 1) as usize;
            if digits.len() <= int_len {
                format!("{sign}{digits}{}", "0".repeat(int_len - digits.len()))
            } else {
                format!("{sign}{}.{}", &digits[..int_len], &digits[int_len..])
            }
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let dot = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{first}{dot}e{esign}{:02}", exp.abs())
    }
}

/// `numeric_out` of what `numeric_in` reads `v` as: every digit of the
/// display scale the literal gave (`1.50`, `1e-3` → `0.001`), no exponent.
pub(crate) fn numeric_out(v: &str) -> Option<String> {
    let s = v.trim_matches(|c: char| c.is_ascii_whitespace());
    if s.eq_ignore_ascii_case("nan") {
        return Some("NaN".to_owned());
    }
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if s.eq_ignore_ascii_case("infinity") || s.eq_ignore_ascii_case("inf") {
        return Some(if neg { "-Infinity" } else { "Infinity" }.to_owned());
    }
    if s.len() > 1 && s.starts_with('0') && s[1..].starts_with(|c: char| c.is_ascii_alphabetic()) {
        // 0x / 0o / 0b integers.
        return None;
    }
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i64>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let clean = |part: &str| -> Option<String> {
        let digits: String = part.chars().filter(|&c| c != '_').collect();
        digits.chars().all(|c| c.is_ascii_digit()).then_some(digits)
    };
    let (int, frac) = (clean(int)?, clean(frac)?);
    if int.is_empty() && frac.is_empty() || exp.abs() > 100_000 {
        return None;
    }
    let dscale = (frac.len() as i64 - exp).max(0) as usize;
    let digits = format!("{int}{frac}");
    let point = int.len() as i64 + exp;
    let (mut int_part, frac_part) = if point <= 0 {
        (
            String::new(),
            format!("{}{digits}", "0".repeat((-point) as usize)),
        )
    } else {
        let point = point as usize;
        if digits.len() <= point {
            (
                format!("{digits}{}", "0".repeat(point - digits.len())),
                String::new(),
            )
        } else {
            (digits[..point].to_owned(), digits[point..].to_owned())
        }
    };
    let frac_part: String = format!("{frac_part:0<dscale$}")[..dscale].to_owned();
    let trimmed = int_part.trim_start_matches('0');
    int_part = if trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    };
    let zero = int_part == "0" && frac_part.chars().all(|c| c == '0');
    let sign = if neg && !zero { "-" } else { "" };
    Some(if dscale > 0 {
        format!("{sign}{int_part}.{frac_part}")
    } else {
        format!("{sign}{int_part}")
    })
}

/// `byteaout` (bytea_output `hex`) of what `byteain` reads.
fn bytea_out(v: &str) -> Option<String> {
    let mut bytes = Vec::new();
    if let Some(hex) = v.strip_prefix("\\x").or_else(|| v.strip_prefix("\\X")) {
        let mut digits = hex.chars().filter(|c| !c.is_ascii_whitespace());
        while let Some(hi) = digits.next() {
            let lo = digits.next()?;
            bytes.push(u8::try_from(hi.to_digit(16)? * 16 + lo.to_digit(16)?).ok()?);
        }
    } else {
        let b = v.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'\\' {
                bytes.push(b[i]);
                i += 1;
            } else if b.get(i + 1) == Some(&b'\\') {
                bytes.push(b'\\');
                i += 2;
            } else {
                let oct = b.get(i + 1..i + 4)?;
                if !(b'0'..=b'3').contains(&oct[0])
                    || !oct[1..].iter().all(|c| (b'0'..=b'7').contains(c))
                {
                    return None;
                }
                bytes.push((oct[0] - b'0') * 64 + (oct[1] - b'0') * 8 + (oct[2] - b'0'));
                i += 4;
            }
        }
    }
    let mut out = String::from("\\x");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    Some(out)
}

/// `uuid_out`: the 32 hex digits, lowercase, grouped 8-4-4-4-12.
fn uuid_out(v: &str) -> Option<String> {
    let s = v.trim();
    let s = s
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(s);
    if s.starts_with('-') || s.ends_with('-') || s.contains("--") {
        return None;
    }
    let hex: String = s.chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let h = hex.to_ascii_lowercase();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// `inet_out` / `cidr_out` (network_out, over inet_net_ntop) of an address
/// written in full, with its mask length.
fn inet_out(v: &str, cidr: bool) -> Option<String> {
    // network_in: an address with a `:` is IPv6.
    if !v.contains(':') {
        let (octets, bits) = if cidr {
            cidr_pton_ipv4(v)?
        } else {
            inet_pton_ipv4(v)?
        };
        let mut out = format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3]);
        if bits != 32 || cidr {
            out.push_str(&format!("/{bits}"));
        }
        return Some(out);
    }
    let s = v.trim();
    let (addr, bits) = match s.split_once('/') {
        Some((a, b)) => (a, Some(b.parse::<u32>().ok()?)),
        None => (s, None),
    };
    let v6: std::net::Ipv6Addr = addr.parse().ok()?;
    let bits = bits.unwrap_or(128);
    if bits > 128 {
        return None;
    }
    let mut out = ipv6_text(v6.segments(), v6.octets());
    if bits != 128 || cidr {
        out.push_str(&format!("/{bits}"));
    }
    Some(out)
}

/// The dotted decimal octets of an IPv4 address and what follows them.
fn dotted_octets(s: &str) -> Option<(Vec<u8>, &str)> {
    let b = s.as_bytes();
    let mut octets = Vec::new();
    let mut i = 0;
    loop {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start || octets.len() == 4 {
            return None;
        }
        octets.push(u8::try_from(s[start..i].parse::<u32>().ok()?).ok()?);
        match b.get(i) {
            Some(b'.') => i += 1,
            _ => return Some((octets, &s[i..])),
        }
    }
}

/// A `/n` suffix of at most 32 (`None`: no suffix, `Some(None)`: invalid).
fn mask_suffix(rest: &str) -> Option<Option<u32>> {
    match rest.strip_prefix('/') {
        None if rest.is_empty() => Some(None),
        Some(bits) if !bits.is_empty() && bits.chars().all(|c| c.is_ascii_digit()) => {
            let bits: u32 = bits.parse().ok()?;
            (bits <= 32).then_some(Some(bits))
        }
        _ => None,
    }
}

/// `inet_net_pton_ipv4`: an inet's octets (abbreviated ones given a mask
/// covering them) and mask length.
fn inet_pton_ipv4(s: &str) -> Option<([u8; 4], u32)> {
    let (octets, rest) = dotted_octets(s)?;
    let bits = match mask_suffix(rest)? {
        Some(bits) => bits,
        None if octets.len() == 4 => 32,
        None => return None,
    };
    if (bits / 8) as usize > octets.len() {
        return None;
    }
    let mut out = [0; 4];
    out[..octets.len()].copy_from_slice(&octets);
    Some((out, bits))
}

/// `inet_cidr_pton_ipv4`: a cidr's octets (dotted decimal or `0x` hex)
/// and mask length — without one, the address class's, widened to the
/// octets given.
fn cidr_pton_ipv4(s: &str) -> Option<([u8; 4], u32)> {
    let (octets, rest) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) if hex.starts_with(|c: char| c.is_ascii_hexdigit()) => {
            let end = hex
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(hex.len());
            let digits = &hex[..end];
            let mut octets = Vec::new();
            for pair in digits.as_bytes().chunks(2) {
                let hi = char::from(pair[0]).to_digit(16)?;
                let lo = match pair.get(1) {
                    Some(c) => char::from(*c).to_digit(16)?,
                    None => 0,
                };
                octets.push(u8::try_from(hi * 16 + lo).ok()?);
            }
            (octets, &hex[end..])
        }
        _ => dotted_octets(s)?,
    };
    if octets.is_empty() || octets.len() > 4 {
        return None;
    }
    let bits = match mask_suffix(rest)? {
        Some(bits) => bits,
        None => {
            let first = octets[0];
            let mut bits = match first {
                240.. => 32,
                224.. => 8,
                192.. => 24,
                128.. => 16,
                _ => 8,
            };
            let given = 8 * octets.len() as u32;
            if bits < given {
                bits = given;
            }
            if bits == 8 && first == 224 {
                bits = 4;
            }
            bits
        }
    };
    let mut out = [0; 4];
    out[..octets.len()].copy_from_slice(&octets);
    // addressOK: no bit set beyond the mask.
    let addr = u32::from_be_bytes(out);
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    (addr & !mask == 0).then_some((out, bits))
}

/// `inet_net_ntop_ipv6`: the longest run of two or more zero words (the
/// first of equal ones) as `::`, an IPv4 tail after six zero words (or
/// five and `ffff`, or seven but not `::1`).
fn ipv6_text(words: [u16; 8], octets: [u8; 16]) -> String {
    // Runs of zero words, as (first word, length).
    let mut best: Option<(usize, usize)> = None;
    let mut cur: Option<(usize, usize)> = None;
    for (i, w) in words.iter().enumerate() {
        if *w == 0 {
            cur = Some(match cur {
                Some((base, len)) => (base, len + 1),
                None => (i, 1),
            });
        } else if let Some(c) = cur.take()
            && best.is_none_or(|b| c.1 > b.1)
        {
            best = Some(c);
        }
    }
    if let Some(c) = cur
        && best.is_none_or(|b| c.1 > b.1)
    {
        best = Some(c);
    }
    let best = best.filter(|b| b.1 >= 2);
    let mut out = String::new();
    for i in 0..8 {
        if let Some((base, len)) = best
            && (base..base + len).contains(&i)
        {
            if i == base {
                out.push(':');
            }
            continue;
        }
        if i != 0 {
            out.push(':');
        }
        if i == 6
            && let Some((0, len)) = best
            && (len == 6 || (len == 7 && words[7] != 1) || (len == 5 && words[5] == 0xffff))
        {
            out.push_str(&format!(
                "{}.{}.{}.{}",
                octets[12], octets[13], octets[14], octets[15]
            ));
            return out;
        }
        out.push_str(&format!("{:x}", words[i]));
    }
    if let Some((base, len)) = best
        && base + len == 8
    {
        out.push(':');
    }
    out
}

/// `macaddr_out` / `macaddr8_out`: the bytes as lowercase hex pairs joined
/// by `:` (a six-byte macaddr8 gets `ff:fe` in the middle).
fn macaddr_out(v: &str, eight: bool) -> Option<String> {
    let hex: String = v
        .trim()
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .collect();
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut hex = hex.to_ascii_lowercase();
    match (hex.len(), eight) {
        (12, false) | (16, true) => {}
        (12, true) => hex = format!("{}fffe{}", &hex[..6], &hex[6..]),
        _ => return None,
    }
    let pairs: Vec<&str> = (0..hex.len() / 2).map(|i| &hex[2 * i..2 * i + 2]).collect();
    Some(pairs.join(":"))
}

/// `bit_out` / `varbit_out`: the bits of a binary (`101`, `b101`) or
/// hexadecimal (`x1F`) literal.
fn bit_out(v: &str) -> Option<String> {
    if let Some(hex) = v.strip_prefix(['x', 'X']) {
        let mut out = String::new();
        for c in hex.chars() {
            out.push_str(&format!("{:04b}", c.to_digit(16)?));
        }
        return Some(out);
    }
    let bin = v.strip_prefix(['b', 'B']).unwrap_or(v);
    bin.chars()
        .all(|c| c == '0' || c == '1')
        .then(|| bin.to_owned())
}

/// `cash_out` of what `cash_in` reads, under the `C` / `en_US` monetary
/// conventions (`$`, `,` grouping, two fraction digits): a third fraction
/// digit of 5 or more rounds away from zero, the rest are dropped.
fn money_out(v: &str) -> Option<String> {
    let mut s = v.trim();
    let mut neg = false;
    if let Some(rest) = s.strip_prefix('-') {
        neg = true;
        s = rest.trim_start();
    }
    if let Some(rest) = s.strip_prefix('$') {
        s = rest.trim_start();
    }
    if !neg && let Some(rest) = s.strip_prefix('-') {
        neg = true;
        s = rest;
    }
    let b = s.as_bytes();
    let (mut value, mut dec, mut seen_dot, mut i) = (0i128, 0, false, 0);
    let mut any = false;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() && (!seen_dot || dec < 2) {
            value = value.checked_mul(10)? + i128::from(c - b'0');
            any = true;
            if seen_dot {
                dec += 1;
            }
        } else if c == b'.' && !seen_dot {
            seen_dot = true;
        } else if c != b',' {
            break;
        }
        i += 1;
    }
    if b.get(i).is_some_and(u8::is_ascii_digit) {
        if b[i] >= b'5' {
            value += 1;
        }
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    if !any || !b[i..].iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    for _ in dec..2 {
        value *= 10;
    }
    if value > i128::from(i64::MAX) {
        return None;
    }
    let int = (value / 100).to_string();
    let mut grouped = String::new();
    for (k, c) in int.chars().enumerate() {
        if k > 0 && (int.len() - k) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    Some(format!(
        "{}${grouped}.{:02}",
        if neg && value != 0 { "-" } else { "" },
        value % 100
    ))
}

/// The numbers of a geometric literal, in order (`float8in` each).
fn all_floats(v: &str) -> Option<Vec<f64>> {
    v.split(|c: char| "()[]{}<>,".contains(c))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| {
            if t.contains(['x', 'X', '_']) {
                None
            } else {
                t.parse::<f64>().ok()
            }
        })
        .collect()
}

/// Exactly `N` numbers of a geometric literal.
fn floats<const N: usize>(v: &str) -> Option<[f64; N]> {
    all_floats(v)?.try_into().ok()
}

/// `pair_encode`: `(x,y)`.
fn point(x: f64, y: f64) -> String {
    format!("({},{})", f8(x), f8(y))
}

/// array_out's quoting of an element: empty, `NULL`, or holding a
/// delimiter, brace, quote, backslash or white space.
fn array_element_text(value: &str, delim: u8) -> String {
    let needs = value.is_empty()
        || value.eq_ignore_ascii_case("null")
        || value.bytes().any(|b| {
            matches!(
                b,
                b'"' | b'\\' | b'{' | b'}' | b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c
            ) || b == delim
        });
    if !needs {
        return value.to_owned();
    }
    let mut out = String::from("\"");
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `array_out` of what `array_in` reads `v` as.
fn array_out(interp: &PgCatalog, elem: PgTypeOid, v: &str, style: Style) -> Option<String> {
    let delim = crate::literal_input::array_delimiter(elem, interp)?;
    let mut elems: Vec<Option<String>> = Vec::new();
    crate::array_input::parse_array(v, delim, &mut |e| {
        elems.push(e.map(str::to_owned));
        Ok(())
    })
    .ok()?;
    let dims = crate::array_input::array_dimensions(v, delim)?;
    if dims.is_empty() {
        return Some("{}".to_owned());
    }
    let texts = elems
        .iter()
        .map(|e| match e {
            None => Some("NULL".to_owned()),
            Some(e) => text_of(interp, elem, e, style).map(|o| array_element_text(&o, delim)),
        })
        .collect::<Option<Vec<String>>>()?;
    let mut out = String::new();
    if dims.iter().any(|&(lower, _)| lower != 1) {
        for &(lower, len) in &dims {
            out.push_str(&format!("[{lower}:{}]", lower + len - 1));
        }
        out.push('=');
    }
    let lens: Vec<usize> = dims
        .iter()
        .map(|&(_, len)| usize::try_from(len).ok())
        .collect::<Option<_>>()?;
    if lens.iter().product::<usize>() != texts.len() {
        return None;
    }
    let delim = delim as char;
    let mut next = texts.into_iter();
    fn nest(out: &mut String, lens: &[usize], next: &mut dyn Iterator<Item = String>, delim: char) {
        out.push('{');
        for i in 0..lens[0] {
            if i > 0 {
                out.push(delim);
            }
            if lens.len() > 1 {
                nest(out, &lens[1..], next, delim);
            } else if let Some(t) = next.next() {
                out.push_str(&t);
            }
        }
        out.push('}');
    }
    nest(&mut out, &lens, &mut next, delim);
    Some(out)
}

/// `range_out` of what `range_in` reads `v` as, discrete ranges in their
/// canonical `[lower,upper)` form.
fn range_out(interp: &PgCatalog, typname: &str, v: &str, style: Style) -> Option<String> {
    let parts = crate::range_input::range_parts(v)?;
    if parts.empty {
        return Some("empty".to_owned());
    }
    // Each bound as an ordered value and its output.
    enum Val {
        Int(i128),
        Num(crate::decimal::Decimal),
        Time(i128),
    }
    let settings = Settings::of(interp);
    let bound = |b: &str| -> Option<(Val, String)> {
        Some(match typname {
            "int4range" | "int8range" => {
                let i = crate::literal_input::parse_pg_integer(b)?;
                (Val::Int(i), i.to_string())
            }
            "numrange" => {
                let out = numeric_out(b)?;
                (Val::Num(crate::decimal::Decimal::parse(&out)?), out)
            }
            "daterange" => {
                let d = dt::date_value(b, &settings)?;
                (Val::Time(d), dt::value_text("date", d, &settings, style)?)
            }
            "tsrange" => {
                let t = dt::timestamp_value(b, &settings)?;
                (
                    Val::Time(t),
                    dt::value_text("timestamp", t, &settings, style)?,
                )
            }
            "tstzrange" => {
                let t = dt::timestamptz_value(b, &settings)?;
                (
                    Val::Time(t),
                    dt::value_text("timestamptz", t, &settings, style)?,
                )
            }
            _ => return None,
        })
    };
    let mut lower = match &parts.lower {
        Some(b) => Some(bound(b)?),
        None => None,
    };
    let mut upper = match &parts.upper {
        Some(b) => Some(bound(b)?),
        None => None,
    };
    let (mut lower_inc, mut upper_inc) = (
        parts.lower_inc && lower.is_some(),
        parts.upper_inc && upper.is_some(),
    );
    // int4range_canonical / int8range_canonical / daterange_canonical.
    let step = |b: &(Val, String)| -> Option<(Val, String)> {
        match b.0 {
            Val::Int(i) => Some((Val::Int(i + 1), (i + 1).to_string())),
            Val::Time(d) if typname == "daterange" => {
                if d == i128::MAX || d == i128::MIN {
                    return Some((Val::Time(d), b.1.clone()));
                }
                Some((
                    Val::Time(d + 1),
                    dt::value_text("date", d + 1, &settings, style)?,
                ))
            }
            _ => None,
        }
    };
    if matches!(typname, "int4range" | "int8range" | "daterange") {
        if let Some(l) = &lower
            && !lower_inc
        {
            lower = Some(step(l)?);
            lower_inc = true;
        }
        if let Some(u) = &upper
            && upper_inc
        {
            upper = Some(step(u)?);
            upper_inc = false;
        }
    }
    if let (Some((l, _)), Some((u, _))) = (&lower, &upper) {
        let order = match (l, u) {
            (Val::Int(a), Val::Int(b)) | (Val::Time(a), Val::Time(b)) => a.cmp(b),
            (Val::Num(a), Val::Num(b)) => a.compare(*b)?,
            _ => return None,
        };
        match order {
            std::cmp::Ordering::Greater => return None,
            std::cmp::Ordering::Equal if !(lower_inc && upper_inc) => {
                return Some("empty".to_owned());
            }
            _ => {}
        }
    }
    let escape = |s: &str| -> String {
        let needs = s.is_empty()
            || s.chars().any(|c| {
                matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || c.is_ascii_whitespace()
            });
        if !needs {
            return s.to_owned();
        }
        let mut out = String::from("\"");
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push(c);
            }
            out.push(c);
        }
        out.push('"');
        out
    };
    Some(format!(
        "{}{},{}{}",
        if lower_inc { '[' } else { '(' },
        lower.map(|(_, s)| escape(&s)).unwrap_or_default(),
        upper.map(|(_, s)| escape(&s)).unwrap_or_default(),
        if upper_inc { ']' } else { ')' },
    ))
}

/// A `jsonb` value as `jsonb_out` prints it.
enum Json {
    Null,
    Bool(bool),
    /// `numeric_out` of the number.
    Number(String),
    String(String),
    Array(Vec<Json>),
    /// Keys unique, ordered by length then bytes (uniqueifyJsonbObject).
    Object(Vec<(String, Json)>),
}

impl Json {
    /// JsonbToCString.
    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => out.push_str(n),
            Json::String(s) => escape_json(out, s),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                out.push('{');
                for (i, (k, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    escape_json(out, k);
                    out.push_str(": ");
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// `escape_json`.
fn escape_json(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The JSON grammar `jsonb_in` reads.
struct JsonParser<'a> {
    b: &'a [u8],
    p: usize,
}

impl JsonParser<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.p), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.p += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.b.get(self.p) == Some(&c) {
            self.p += 1;
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.ws();
        match *self.b.get(self.p)? {
            b'{' => {
                self.p += 1;
                let mut pairs: Vec<(String, Json)> = Vec::new();
                if !self.eat(b'}') {
                    loop {
                        self.ws();
                        let key = self.string()?;
                        if !self.eat(b':') {
                            return None;
                        }
                        let value = self.value()?;
                        pairs.push((key, value));
                        if self.eat(b',') {
                            continue;
                        }
                        if self.eat(b'}') {
                            break;
                        }
                        return None;
                    }
                }
                // The last of equal keys wins.
                let mut unique: Vec<(String, Json)> = Vec::new();
                for (k, value) in pairs {
                    unique.retain(|(u, _)| *u != k);
                    unique.push((k, value));
                }
                unique.sort_by(|(a, _), (b, _)| {
                    a.len()
                        .cmp(&b.len())
                        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
                });
                Some(Json::Object(unique))
            }
            b'[' => {
                self.p += 1;
                let mut items = Vec::new();
                if !self.eat(b']') {
                    loop {
                        items.push(self.value()?);
                        if self.eat(b',') {
                            continue;
                        }
                        if self.eat(b']') {
                            break;
                        }
                        return None;
                    }
                }
                Some(Json::Array(items))
            }
            b'"' => Some(Json::String(self.string()?)),
            b't' => self.word("true", Json::Bool(true)),
            b'f' => self.word("false", Json::Bool(false)),
            b'n' => self.word("null", Json::Null),
            _ => self.number(),
        }
    }

    fn word(&mut self, w: &str, value: Json) -> Option<Json> {
        if self.b[self.p..].starts_with(w.as_bytes()) {
            self.p += w.len();
            // A keyword ends at a non-alphanumeric character.
            if self
                .b
                .get(self.p)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
            {
                return None;
            }
            Some(value)
        } else {
            None
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.p;
        let digits = |p: &mut Self| {
            let s = p.p;
            while p.b.get(p.p).is_some_and(u8::is_ascii_digit) {
                p.p += 1;
            }
            p.p > s
        };
        if self.b.get(self.p) == Some(&b'-') {
            self.p += 1;
        }
        if self.b.get(self.p) == Some(&b'0') {
            self.p += 1;
        } else if !digits(self) {
            return None;
        }
        if self.b.get(self.p) == Some(&b'.') {
            self.p += 1;
            if !digits(self) {
                return None;
            }
        }
        if matches!(self.b.get(self.p), Some(b'e' | b'E')) {
            self.p += 1;
            if matches!(self.b.get(self.p), Some(b'+' | b'-')) {
                self.p += 1;
            }
            if !digits(self) {
                return None;
            }
        }
        if self
            .b
            .get(self.p)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.')
        {
            return None;
        }
        let text = std::str::from_utf8(&self.b[start..self.p]).ok()?;
        Some(Json::Number(numeric_out(text)?))
    }

    fn string(&mut self) -> Option<String> {
        if self.b.get(self.p) != Some(&b'"') {
            return None;
        }
        self.p += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.p)?;
            self.p += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.p)?;
                    self.p += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut code = self.hex4()?;
                            if (0xD800..0xDC00).contains(&code) {
                                if self.b.get(self.p..self.p + 2) != Some(b"\\u") {
                                    return None;
                                }
                                self.p += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return None;
                                }
                                code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                            }
                            // jsonb refuses \u0000 and a lone low surrogate.
                            let ch = char::from_u32(code).filter(|&c| c != '\0')?;
                            let mut buf = [0; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return None,
                    }
                }
                c if c < 0x20 => return None,
                c => out.push(c),
            }
        }
        String::from_utf8(out).ok()
    }

    fn hex4(&mut self) -> Option<u32> {
        let s = std::str::from_utf8(self.b.get(self.p..self.p + 4)?).ok()?;
        self.p += 4;
        u32::from_str_radix(s, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn floats_print_their_shortest_digits() {
        // float8out / float4out on PostgreSQL 19.
        for (v, single, out) in [
            ("1.50", false, "1.5"),
            ("0.1", false, "0.1"),
            ("1e15", false, "1e+15"),
            ("123456789012345", false, "123456789012345"),
            ("0.0001", false, "0.0001"),
            ("0.00001", false, "1e-05"),
            ("-2.5e-10", false, "-2.5e-10"),
            ("1e300", false, "1e+300"),
            ("-0", false, "-0"),
            ("NaN", false, "NaN"),
            ("-inf", false, "-Infinity"),
            ("100000", true, "100000"),
            ("1000000", true, "1e+06"),
            ("0.1", true, "0.1"),
            ("3.14159265358979", true, "3.1415927"),
        ] {
            assert_eq!(super::float_out(v, single).as_deref(), Some(out), "{v}");
        }
    }

    #[test]
    fn numerics_keep_their_display_scale() {
        for (v, out) in [
            ("1.50", "1.50"),
            ("  +007.0 ", "7.0"),
            (".5", "0.5"),
            ("1e3", "1000"),
            ("1.5e-3", "0.0015"),
            ("-0.00", "0.00"),
            ("12.5E1", "125"),
            ("1_000.25", "1000.25"),
            ("inf", "Infinity"),
            ("-Infinity", "-Infinity"),
            ("nan", "NaN"),
        ] {
            assert_eq!(super::numeric_out(v).as_deref(), Some(out), "{v}");
        }
    }
}
