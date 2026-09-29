//! A port of PostgreSQL 18's `array_in` grammar (arrayfuncs.c:
//! `ReadArrayDimensions`, `ReadDimensionInt`, `ReadArrayStr`,
//! `ReadArrayToken`), used by [`crate::literal_input`] to validate array
//! literals element by element.
//!
//! The walk reports every element — its de-escaped text, or `None` for an
//! unquoted `NULL` — to a callback *at the point `array_in` would call the
//! element's input function*, so an element error surfaces before any
//! structural error further right in the string, exactly like PG.
//! Every structural message is PG's verbatim primary message (the DETAIL
//! line PG attaches to `malformed array literal` is not reproduced).

/// `MAXDIM` (utils/array.h).
const MAXDIM: usize = 6;

/// `MaxArraySize` = `MaxAllocSize / sizeof(Datum)` (utils/array.h).
const MAX_ARRAY_SIZE: i64 = 0x3fff_ffff / 8;

/// `scanner_isspace` (parser/scansup.c): the whitespace `array_in` skips.
fn scanner_isspace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c')
}

enum Token {
    LevelStart,
    LevelEnd,
    Delim,
    Elem(String),
    ElemNull,
}

struct Parser<'a> {
    b: &'a [u8],
    p: usize,
    delim: u8,
    content: &'a str,
}

impl Parser<'_> {
    fn malformed(&self) -> String {
        format!("malformed array literal: \"{}\"", self.content)
    }

    fn at(&self, i: usize) -> u8 {
        self.b.get(i).copied().unwrap_or(0)
    }

    /// `ReadDimensionInt`: `strtol` over an optional sign and decimal
    /// digits. Returns the value and the position after it — unchanged
    /// when no digits were read.
    fn read_dimension_int(&self, start: usize) -> Result<(i64, usize), String> {
        let c = self.at(start);
        if !(c.is_ascii_digit() || c == b'-' || c == b'+') {
            return Ok((0, start));
        }
        let mut p = start;
        let negative = c == b'-';
        if !c.is_ascii_digit() {
            p += 1;
        }
        let digits_start = p;
        let mut v: i128 = 0;
        while self.at(p).is_ascii_digit() {
            v = (v * 10 + i128::from(self.at(p) - b'0')).min(i128::from(i64::MAX) + 2);
            p += 1;
        }
        if p == digits_start {
            // strtol found no conversion: endptr is the original pointer.
            return Ok((0, start));
        }
        let v = if negative { -v } else { v };
        if v > i128::from(i32::MAX) || v < i128::from(i32::MIN) {
            return Err("array bound is out of integer range".to_string());
        }
        Ok((v as i64, p))
    }

    /// `ReadArrayDimensions`: the optional `[lo:hi]…` prefix. Returns the
    /// explicit dimension lengths (empty when there are none).
    fn read_dimensions(&mut self) -> Result<Vec<i64>, String> {
        let mut dims = Vec::new();
        loop {
            while scanner_isspace(self.at(self.p)) {
                self.p += 1;
            }
            if self.at(self.p) != b'[' {
                break;
            }
            self.p += 1;
            if dims.len() >= MAXDIM {
                return Err(format!(
                    "number of array dimensions exceeds the maximum allowed ({MAXDIM})"
                ));
            }
            let q = self.p;
            let (i, np) = self.read_dimension_int(self.p)?;
            self.p = np;
            if self.p == q {
                return Err(self.malformed());
            }
            let (lb, ub) = if self.at(self.p) == b':' {
                self.p += 1;
                let q = self.p;
                let (ub, np) = self.read_dimension_int(self.p)?;
                self.p = np;
                if self.p == q {
                    return Err(self.malformed());
                }
                (i, ub)
            } else {
                (1, i)
            };
            if self.at(self.p) != b']' {
                return Err(self.malformed());
            }
            self.p += 1;
            if ub < lb {
                return Err("upper bound cannot be less than lower bound".to_string());
            }
            if ub == i64::from(i32::MAX) {
                return Err(format!("array upper bound is too large: {ub}"));
            }
            let len = ub - lb + 1;
            if len > i64::from(i32::MAX) {
                return Err(format!(
                    "array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"
                ));
            }
            dims.push(len);
        }
        Ok(dims)
    }

    /// `ReadArrayToken`: one token starting at the scan point.
    fn read_token(&mut self) -> Result<Token, String> {
        let mut p = self.p;
        // Identify the token type, skipping leading whitespace.
        loop {
            match self.at(p) {
                0 => return Err(self.malformed()),
                b'{' => {
                    self.p = p + 1;
                    return Ok(Token::LevelStart);
                }
                b'}' => {
                    self.p = p + 1;
                    return Ok(Token::LevelEnd);
                }
                b'"' => {
                    p += 1;
                    return self.quoted_element(p);
                }
                c if c == self.delim => {
                    self.p = p + 1;
                    return Ok(Token::Delim);
                }
                c if scanner_isspace(c) => p += 1,
                _ => return self.unquoted_element(p),
            }
        }
    }

    fn quoted_element(&mut self, mut p: usize) -> Result<Token, String> {
        let mut buf = Vec::new();
        loop {
            match self.at(p) {
                0 => return Err(self.malformed()),
                b'\\' => {
                    p += 1;
                    if self.at(p) == 0 {
                        return Err(self.malformed());
                    }
                    buf.push(self.at(p));
                    p += 1;
                }
                b'"' => {
                    // The next non-whitespace must be the delimiter or a
                    // brace, else the element is incorrectly quoted.
                    p += 1;
                    while self.at(p) != 0 {
                        let c = self.at(p);
                        if c == self.delim || c == b'}' || c == b'{' {
                            self.p = p;
                            return Ok(Token::Elem(String::from_utf8_lossy(&buf).into_owned()));
                        }
                        if !scanner_isspace(c) {
                            return Err(self.malformed());
                        }
                        p += 1;
                    }
                    return Err(self.malformed());
                }
                c => {
                    buf.push(c);
                    p += 1;
                }
            }
        }
    }

    fn unquoted_element(&mut self, mut p: usize) -> Result<Token, String> {
        let mut buf = Vec::new();
        // Trailing whitespace is dropped; `dstlen` tracks the prefix known
        // not to be trailing whitespace (escaped characters count as
        // non-whitespace).
        let mut dstlen = 0;
        let mut has_escapes = false;
        loop {
            match self.at(p) {
                0 => return Err(self.malformed()),
                b'{' | b'"' => return Err(self.malformed()),
                b'\\' => {
                    p += 1;
                    if self.at(p) == 0 {
                        return Err(self.malformed());
                    }
                    buf.push(self.at(p));
                    p += 1;
                    dstlen = buf.len();
                    has_escapes = true;
                }
                c if c == self.delim || c == b'}' => {
                    buf.truncate(dstlen);
                    self.p = p;
                    // `Array_nulls` (default on): an unescaped, unquoted
                    // `NULL` in any case is a null element.
                    if !has_escapes && buf.eq_ignore_ascii_case(b"NULL") {
                        return Ok(Token::ElemNull);
                    }
                    return Ok(Token::Elem(String::from_utf8_lossy(&buf).into_owned()));
                }
                c => {
                    buf.push(c);
                    if !scanner_isspace(c) {
                        dstlen = buf.len();
                    }
                    p += 1;
                }
            }
        }
    }

    /// `ReadArrayStr`: the braced body, checking nesting and dimension
    /// consistency and handing each element to `on_elem`.
    fn read_array_str(
        &mut self,
        explicit_dims: &[i64],
        on_elem: &mut dyn FnMut(Option<&str>) -> Result<(), String>,
    ) -> Result<(), String> {
        let dimensions_specified = !explicit_dims.is_empty();
        let mut ndim = explicit_dims.len();
        let mut dim = [-1i64; MAXDIM];
        dim[..ndim].copy_from_slice(explicit_dims);
        let mut nelems = [0i64; MAXDIM];
        let mut nest_level = 0usize;
        let mut ndim_frozen = dimensions_specified;
        let mut expect_delim = false;
        loop {
            match self.read_token()? {
                Token::LevelStart => {
                    if expect_delim {
                        return Err(self.malformed());
                    }
                    if nest_level >= MAXDIM {
                        return Err(format!(
                            "number of array dimensions exceeds the maximum allowed ({MAXDIM})"
                        ));
                    }
                    nelems[nest_level] = 0;
                    nest_level += 1;
                    if nest_level > ndim {
                        if ndim_frozen {
                            return Err(self.malformed());
                        }
                        ndim = nest_level;
                    }
                }
                Token::LevelEnd => {
                    if nelems[nest_level - 1] > 0 && !expect_delim {
                        return Err(self.malformed());
                    }
                    nest_level -= 1;
                    if nest_level > 0 {
                        nelems[nest_level - 1] += 1;
                    }
                    if dim[nest_level] < 0 {
                        dim[nest_level] = nelems[nest_level];
                    } else if nelems[nest_level] != dim[nest_level] {
                        return Err(self.malformed());
                    }
                    expect_delim = true;
                }
                Token::Delim => {
                    if !expect_delim {
                        return Err(self.malformed());
                    }
                    expect_delim = false;
                }
                tok @ (Token::Elem(_) | Token::ElemNull) => {
                    if expect_delim {
                        return Err(self.malformed());
                    }
                    match &tok {
                        Token::Elem(s) => on_elem(Some(s))?,
                        _ => on_elem(None)?,
                    }
                    ndim_frozen = true;
                    if nest_level != ndim {
                        return Err(self.malformed());
                    }
                    nelems[nest_level - 1] += 1;
                    expect_delim = true;
                }
            }
            if nest_level == 0 {
                return Ok(());
            }
        }
    }
}

/// Walk `content` with `array_in`'s grammar under element delimiter
/// `delim`, calling `on_elem` for each element in input order (`None` for
/// a NULL element). The first error — structural, or returned by
/// `on_elem` — is returned verbatim.
pub(crate) fn parse_array(
    content: &str,
    delim: u8,
    on_elem: &mut dyn FnMut(Option<&str>) -> Result<(), String>,
) -> Result<(), String> {
    let mut p = Parser {
        b: content.as_bytes(),
        p: 0,
        delim,
        content,
    };
    let dims = p.read_dimensions()?;
    if dims.is_empty() {
        if p.at(p.p) != b'{' {
            return Err(p.malformed());
        }
    } else {
        if p.at(p.p) != b'=' {
            return Err(p.malformed());
        }
        p.p += 1;
        while scanner_isspace(p.at(p.p)) {
            p.p += 1;
        }
        if p.at(p.p) != b'{' {
            return Err(p.malformed());
        }
    }
    p.read_array_str(&dims, on_elem)?;
    // Only whitespace may follow the closing brace.
    if p.b[p.p..].iter().any(|&c| !scanner_isspace(c)) {
        return Err(p.malformed());
    }
    Ok(())
}

/// True unless `content` provably parses (as a `,`-delimited array) to an
/// array with no NULL element. An unquoted, unescaped `NULL` token (any
/// case) is a NULL element; `"NULL"` is the string. A literal that doesn't
/// parse is reported as possibly-NULL (conservative). `box` arrays use `;`
/// as their delimiter, so a literal that also parses that way is checked
/// under both.
pub(crate) fn may_contain_null(content: &str) -> bool {
    let scan = |delim: u8| -> Option<bool> {
        let mut saw_null = false;
        parse_array(content, delim, &mut |e| {
            saw_null |= e.is_none();
            Ok(())
        })
        .ok()?;
        Some(saw_null)
    };
    match scan(b',') {
        None | Some(true) => true,
        Some(false) => content.contains(';') && scan(b';') == Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elems(content: &str) -> Result<Vec<Option<String>>, String> {
        let mut out = Vec::new();
        parse_array(content, b',', &mut |e| {
            out.push(e.map(str::to_owned));
            Ok(())
        })?;
        Ok(out)
    }

    #[test]
    fn element_extraction() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(elems("{1,2}").unwrap(), vec![s("1"), s("2")]);
        assert_eq!(elems(" { a , b } ").unwrap(), vec![s("a"), s("b")]);
        assert_eq!(
            elems(r#"{NULL, null ,"NULL",\NULL}"#).unwrap(),
            vec![None, None, s("NULL"), s("NULL")]
        );
        assert_eq!(elems(r#"{"a\"b",c\,d}"#).unwrap(), vec![s("a\"b"), s("c,d")]);
        assert_eq!(elems(r"{1\ }").unwrap(), vec![s("1 ")]);
        assert_eq!(elems("{{1,2},{3,4}}").unwrap().len(), 4);
        assert_eq!(elems("[0:1]={1,2}").unwrap().len(), 2);
        assert!(elems("{}").unwrap().is_empty());
        assert!(elems("{{},{}}").unwrap().is_empty());
    }

    #[test]
    fn structural_errors() {
        for bad in [
            "{1,2", "{{1},{2,3}}", "{1,}", "{,1}", "{1}}", "{1,{2}}", "{{1},{}}", "{\"a\"b}",
            "{a\"b\"}", " {1} x", "[1]{1}", "[a]={1}", "[1:]={1}", "[1={1}", "[1]= [1]",
            "[ 1]={1}", "[0x1]={1}", "[1:3]={1,2}", "{\"a}", "{a\\", "1", "",
        ] {
            assert_eq!(
                elems(bad).unwrap_err(),
                format!("malformed array literal: \"{bad}\""),
                "{bad:?}"
            );
        }
        assert_eq!(
            elems("[2:1]={1}").unwrap_err(),
            "upper bound cannot be less than lower bound"
        );
        assert_eq!(
            elems("[1:2147483647]={1}").unwrap_err(),
            "array upper bound is too large: 2147483647"
        );
        assert_eq!(
            elems("[1:99999999999]={1}").unwrap_err(),
            "array bound is out of integer range"
        );
        assert_eq!(
            elems("{{{{{{{1}}}}}}}").unwrap_err(),
            "number of array dimensions exceeds the maximum allowed (6)"
        );
    }

    #[test]
    fn null_detection() {
        assert!(may_contain_null("{1,NULL}"));
        assert!(may_contain_null("{1, null }"));
        assert!(!may_contain_null("{1,\"NULL\"}"));
        assert!(!may_contain_null("{1,2}"));
        assert!(!may_contain_null("{}"));
        assert!(may_contain_null("{1,2"));
        assert!(may_contain_null("{(1,2),(3,4);NULL}"));
        assert!(!may_contain_null("{\"a;b\"}"));
    }
}
