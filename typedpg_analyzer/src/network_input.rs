//! A port of PostgreSQL 18's `inet` / `cidr` input (network.c
//! `network_in` + `addressOK`, and inet_net_pton.c's
//! `inet_net_pton_ipv4`, `inet_cidr_pton_ipv4`, `inet_cidr_pton_ipv6`,
//! `getbits`, `getv4`), used by [`crate::literal_input`].
//!
//! The parsers work on the raw string — PG trims no whitespace here.

/// `inet_cidr_pton_ipv4`: classful-default network parsing for cidr
/// (hex nybble strings, abbreviated dotted forms, optional `/bits`).
/// Returns the address bytes and the mask length.
fn cidr_pton_ipv4(src: &[u8]) -> Option<([u8; 4], i32)> {
    let at = |i: usize| src.get(i).copied().unwrap_or(0);
    let mut dst = [0u8; 4];
    let mut n = 0usize; // bytes written
    let mut i = 0usize;
    let mut ch = at(i);
    i += 1;
    if ch == b'0' && matches!(at(i), b'x' | b'X') && at(i + 1).is_ascii_hexdigit() {
        // Hexadecimal: eat the nybble string.
        i += 1;
        let mut dirty = 0;
        let mut tmp = 0u8;
        loop {
            ch = at(i);
            i += 1;
            if ch == 0 || !ch.is_ascii_hexdigit() {
                break;
            }
            let v = (ch as char).to_digit(16)? as u8;
            tmp = if dirty == 0 { v } else { (tmp << 4) | v };
            dirty += 1;
            if dirty == 2 {
                *dst.get_mut(n)? = tmp;
                n += 1;
                dirty = 0;
            }
        }
        if dirty != 0 {
            *dst.get_mut(n)? = tmp << 4;
            n += 1;
        }
    } else if ch.is_ascii_digit() {
        // Decimal: eat the dotted digit string.
        loop {
            let mut tmp = 0u32;
            loop {
                tmp = tmp * 10 + u32::from(ch - b'0');
                if tmp > 255 {
                    return None;
                }
                ch = at(i);
                i += 1;
                if ch == 0 || !ch.is_ascii_digit() {
                    break;
                }
            }
            *dst.get_mut(n)? = tmp as u8;
            n += 1;
            if ch == 0 || ch == b'/' {
                break;
            }
            if ch != b'.' {
                return None;
            }
            ch = at(i);
            i += 1;
            if !ch.is_ascii_digit() {
                return None;
            }
        }
    } else {
        return None;
    }
    let mut bits = -1i32;
    if ch == b'/' && at(i).is_ascii_digit() && n > 0 {
        ch = at(i);
        i += 1;
        bits = 0;
        loop {
            bits = bits.saturating_mul(10).saturating_add(i32::from(ch - b'0'));
            ch = at(i);
            i += 1;
            if ch == 0 || !ch.is_ascii_digit() {
                break;
            }
        }
        if ch != 0 || bits > 32 {
            return None;
        }
    }
    if ch != 0 || n == 0 {
        return None;
    }
    if bits == -1 {
        bits = match dst[0] {
            240.. => 32,
            224.. => 8,
            192.. => 24,
            128.. => 16,
            _ => 8,
        };
        if bits < (n as i32) * 8 {
            bits = (n as i32) * 8;
        }
        if bits == 8 && dst[0] == 224 {
            bits = 4;
        }
    }
    // Extending the network to cover the mask only writes zero bytes.
    Some((dst, bits))
}

/// `inet_net_pton_ipv4`: dotted decimal octets (possibly abbreviated when
/// a `/bits` covers them) with an optional `/bits`.
fn inet_pton_ipv4(src: &[u8]) -> Option<([u8; 4], i32)> {
    let at = |i: usize| src.get(i).copied().unwrap_or(0);
    let mut dst = [0u8; 4];
    let mut n = 0usize;
    let mut i = 0usize;
    let mut ch;
    loop {
        ch = at(i);
        i += 1;
        if !ch.is_ascii_digit() {
            break;
        }
        let mut tmp = 0u32;
        loop {
            tmp = tmp * 10 + u32::from(ch - b'0');
            if tmp > 255 {
                return None;
            }
            ch = at(i);
            i += 1;
            if ch == 0 || !ch.is_ascii_digit() {
                break;
            }
        }
        *dst.get_mut(n)? = tmp as u8;
        n += 1;
        if ch == 0 || ch == b'/' {
            break;
        }
        if ch != b'.' {
            return None;
        }
    }
    let mut bits = -1i32;
    if ch == b'/' && at(i).is_ascii_digit() && n > 0 {
        ch = at(i);
        i += 1;
        bits = 0;
        loop {
            bits = bits.saturating_mul(10).saturating_add(i32::from(ch - b'0'));
            ch = at(i);
            i += 1;
            if ch == 0 || !ch.is_ascii_digit() {
                break;
            }
        }
        if ch != 0 || bits > 32 {
            return None;
        }
    }
    if ch != 0 {
        return None;
    }
    if bits == -1 {
        if n == 4 {
            bits = 32;
        } else {
            return None;
        }
    }
    if n == 0 || (bits / 8) as usize > n {
        return None;
    }
    Some((dst, bits))
}

/// `getbits`: a `/bits` suffix of at most 128, no leading zeros.
fn getbits(src: &[u8]) -> Option<i32> {
    let mut val = 0i32;
    let mut n = 0;
    for &ch in src {
        if !ch.is_ascii_digit() {
            return None;
        }
        if n != 0 && val == 0 {
            return None;
        }
        n += 1;
        val = val * 10 + i32::from(ch - b'0');
        if val > 128 {
            return None;
        }
    }
    (n != 0).then_some(val)
}

/// `getv4`: an embedded dotted IPv4 tail (no leading zeros), optionally
/// followed by `/bits`. As in PG it is lax: up to 4 octets, missing ones
/// (and empty ones, `1..2`) are zero — `'::1.2'::inet` is `::1.2.0.0`.
/// Returns the 4 bytes and the bits, if any.
fn getv4(src: &[u8]) -> Option<([u8; 4], Option<i32>)> {
    let mut dst = [0u8; 4];
    let mut n = 0usize;
    let mut val = 0u32;
    let mut digits = 0;
    for (k, &ch) in src.iter().enumerate() {
        if ch.is_ascii_digit() {
            if digits != 0 && val == 0 {
                return None;
            }
            digits += 1;
            val = val * 10 + u32::from(ch - b'0');
            if val > 255 {
                return None;
            }
            continue;
        }
        if ch == b'.' || ch == b'/' {
            if n > 3 {
                return None;
            }
            dst[n] = val as u8;
            n += 1;
            if ch == b'/' {
                return Some((dst, Some(getbits(&src[k + 1..])?)));
            }
            val = 0;
            digits = 0;
            continue;
        }
        return None;
    }
    if digits == 0 || n > 3 {
        return None;
    }
    dst[n] = val as u8;
    Some((dst, None))
}

/// `inet_cidr_pton_ipv6`: RFC 4291 text form with at most one `::`, an
/// optional trailing dotted quad, and an optional `/bits`.
fn pton_ipv6(src: &[u8]) -> Option<([u8; 16], i32)> {
    let at = |i: usize| src.get(i).copied().unwrap_or(0);
    let mut tmp = [0u8; 16];
    let mut tp = 0usize;
    let mut colonp: Option<usize> = None;
    let mut i = 0usize;
    if at(0) == b':' {
        if at(1) != b':' {
            return None;
        }
        i = 1;
    }
    let mut curtok = i;
    let mut saw_xdigit = false;
    let mut val = 0u32;
    let mut digits = 0;
    let mut bits = -1i32;
    loop {
        let ch = at(i);
        i += 1;
        if ch == 0 {
            break;
        }
        if let Some(d) = (ch as char).to_digit(16) {
            val = (val << 4) | d;
            digits += 1;
            if digits > 4 {
                return None;
            }
            saw_xdigit = true;
            continue;
        }
        if ch == b':' {
            curtok = i;
            if !saw_xdigit {
                if colonp.is_some() {
                    return None;
                }
                colonp = Some(tp);
                continue;
            } else if at(i) == 0 {
                return None;
            }
            if tp + 2 > 16 {
                return None;
            }
            tmp[tp] = (val >> 8) as u8;
            tmp[tp + 1] = val as u8;
            tp += 2;
            saw_xdigit = false;
            digits = 0;
            val = 0;
            continue;
        }
        if ch == b'.' && tp + 4 <= 16 {
            let end = src[curtok..]
                .iter()
                .position(|&c| c == 0)
                .map_or(src.len(), |e| curtok + e);
            if let Some((v4, v4bits)) = getv4(&src[curtok..end]) {
                tmp[tp..tp + 4].copy_from_slice(&v4);
                tp += 4;
                saw_xdigit = false;
                if let Some(b) = v4bits {
                    bits = b;
                }
                break;
            }
        }
        if ch == b'/'
            && let Some(b) = getbits(&src[i..])
        {
            bits = b;
            break;
        }
        return None;
    }
    if saw_xdigit {
        if tp + 2 > 16 {
            return None;
        }
        tmp[tp] = (val >> 8) as u8;
        tmp[tp + 1] = val as u8;
        tp += 2;
    }
    if bits == -1 {
        bits = 128;
    }
    if let Some(c) = colonp {
        if tp == 16 {
            return None;
        }
        let n = tp - c;
        for k in 1..=n {
            tmp[16 - k] = tmp[c + n - k];
            tmp[c + n - k] = 0;
        }
        tp = 16;
    }
    if tp != 16 {
        return None;
    }
    Some((tmp, bits))
}

/// `addressOK`: no bits set to the right of the mask.
fn address_ok(addr: &[u8], bits: i32) -> bool {
    let maxbits = addr.len() as i32 * 8;
    if bits == maxbits {
        return true;
    }
    let mut byte = (bits / 8) as usize;
    let nbits = bits % 8;
    let mut mask: u8 = 0xff;
    if bits != 0 {
        mask >>= nbits;
    }
    while byte < addr.len() {
        if addr[byte] & mask != 0 {
            return false;
        }
        mask = 0xff;
        byte += 1;
    }
    true
}

/// Mirrors `network_in`: IPv6 when the string contains a `:`, IPv4
/// otherwise; cidr values must have no bits set right of the mask.
pub(crate) fn validate(content: &str, is_cidr: bool) -> Result<(), String> {
    let name = if is_cidr { "cidr" } else { "inet" };
    let syntax = || crate::pgmsg::invalid_input_syntax_for_type(name, content);
    let src = content.as_bytes();
    let (addr, bits): (Vec<u8>, i32) = if content.contains(':') {
        let (a, b) = pton_ipv6(src).ok_or_else(syntax)?;
        (a.to_vec(), b)
    } else if is_cidr {
        let (a, b) = cidr_pton_ipv4(src).ok_or_else(syntax)?;
        (a.to_vec(), b)
    } else {
        let (a, b) = inet_pton_ipv4(src).ok_or_else(syntax)?;
        (a.to_vec(), b)
    };
    if bits < 0 || bits > addr.len() as i32 * 8 {
        return Err(syntax());
    }
    if is_cidr && !address_ok(&addr, bits) {
        return Err(format!("invalid cidr value: \"{content}\""));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn inet_inputs() {
        for ok in [
            "192.168.0.1",
            "192.168.0.1/24",
            "192.168/16",
            "10/8",
            "1.2.3.04",
            "::1",
            "::",
            "fe80::1/64",
            "::ffff:192.168.0.1",
            "1:2:3:4:5:6:7:8",
            "1:2:3:4:5:6:7::",
            "::1/0",
            "::1.2",
            "::1..2",
            "::1.2/120",
            "1:2:3:4:5:6:1.2.3.4",
            "00.1.2.3",
            "1.2.3.4/024",
        ] {
            assert!(validate(ok, false).is_ok(), "{ok:?} should be valid inet");
        }
        for bad in [
            "",
            "42",
            "192.168",
            "256.1.1.1",
            "192.168.0.1/33",
            "1.2.3.4/033",
            "10/24",
            " 1.2.3.4",
            "1.2.3.4 ",
            "1.2.3.4/",
            "1.2.3.4/ 8",
            "1.2.3.4.5",
            "hello",
            "1:2:3:4:5:6:7:8:9",
            "1:2:3:4:5:6:7:8::",
            "1::2::3",
            ":1",
            "::ffff:01.2.3.4",
            "::1/129",
            "::1/064",
            "::1.2.3.",
            "::1.2.3.4.5",
            "1:2:3:4:5:6:7:1.2.3.4",
            "::1.2.3.4/",
            "::1/",
            "1::/64x",
            "1.2.3.4/99999999999",
        ] {
            assert_eq!(
                validate(bad, false).unwrap_err(),
                format!("invalid input syntax for type inet: \"{bad}\""),
            );
        }
    }

    #[test]
    fn cidr_inputs() {
        for ok in [
            "10/8",
            "10",
            "10.0/8",
            "10.1/16",
            "192.168.1",
            "128.1",
            "224.1.2.3",
            "192.168.0.0/24",
            "1.2.3.4/32",
            "0.0.0.0/0",
            "0x0a/8",
            "0x0a0b",
            "0xa",
            "fe80::/64",
            "::1.2.3.0/120",
        ] {
            assert!(validate(ok, true).is_ok(), "{ok:?} should be valid cidr");
        }
        for bad in [
            "x/8",
            " 10/8",
            "10/8 ",
            "1.2.3.4/33",
            "1.2.3.4.5",
            "0x0a0b0c0d0e",
        ] {
            assert_eq!(
                validate(bad, true).unwrap_err(),
                format!("invalid input syntax for type cidr: \"{bad}\""),
            );
        }
        for bad in [
            "1.2.3.4/24",
            "10.1.2.3/8",
            "10.1/8",
            "1.0.0.0/0",
            "::1/64",
            "fe80::1/64",
            "1::/0",
            "::1.2.3.4/120",
        ] {
            assert_eq!(
                validate(bad, true).unwrap_err(),
                format!("invalid cidr value: \"{bad}\""),
            );
        }
    }
}
