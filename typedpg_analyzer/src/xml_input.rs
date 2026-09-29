//! A conservative well-formedness check mirroring `xml_in` (xml.c) under
//! the default `xmloption = content`: PG's own `parse_xml_decl` for an
//! optional leading XML declaration, then libxml2's
//! `xmlParseBalancedChunkMemory` for the rest.
//!
//! libxml2's parser is far larger than what is modelled here, so this only
//! rejects the unambiguous well-formedness violations (each verified
//! against PostgreSQL 18): unbalanced / mismatched / malformed tags,
//! malformed or duplicate attributes, `<` in attribute values, bad entity
//! and character references (only the five predefined entities exist
//! without a DTD), malformed comments / CDATA / processing instructions,
//! `]]>` in text, and characters outside XML's `Char` production.
//! Anything involving a DOCTYPE (parsed as a document), namespaces, or
//! libxml's limits is accepted unchecked.

/// Why a literal can't be accepted with certainty.
enum Verdict {
    /// A well-formedness error PG reports with this message.
    Invalid(&'static str),
    /// Outside what is modelled — accept.
    Unsure,
}

const CONTENT_ERR: &str = "invalid XML content";
const DECL_ERR: &str = "invalid XML content: invalid XML declaration";

/// libxml's `xmlIsBlank_ch`.
fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// XML `NameStartChar` (non-ASCII is treated as a name character: a
/// document PG accepts only has non-ASCII in names where names allow it).
fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == ':' || !c.is_ascii()
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '.' || c == '-'
}

/// XML 1.0 `Char`.
fn is_xml_char(c: u32) -> bool {
    matches!(c, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

/// Validate `content` as `xml_in` would; `Err` carries PG's message.
pub(crate) fn validate(content: &str) -> Result<(), String> {
    match check(content) {
        Ok(()) | Err(Verdict::Unsure) => Ok(()),
        Err(Verdict::Invalid(msg)) => Err(msg.to_string()),
    }
}

fn check(content: &str) -> Result<(), Verdict> {
    let decl_len = parse_xml_decl(content.as_bytes())?;
    let rest = &content[decl_len..];
    if doctype_in_content(rest.as_bytes()) {
        return Err(Verdict::Unsure);
    }
    Parser {
        s: rest,
        p: 0,
        stack: Vec::new(),
    }
    .content()
}

/// Port of xml.c's `parse_xml_decl`: the byte length of a leading
/// `<?xml …?>` declaration (0 if there is none).
fn parse_xml_decl(s: &[u8]) -> Result<usize, Verdict> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    if !s.starts_with(b"<?xml") {
        return Ok(0);
    }
    // `<?xml-stylesheet …?>` etc. is a PI, not a declaration.
    match at(5) {
        c if c >= 0x80 => return Err(Verdict::Unsure),
        c if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b':') => return Ok(0),
        _ => {}
    }
    let decl_err = Err(Verdict::Invalid(DECL_ERR));
    let skip = |mut p: usize| {
        while is_blank(at(p)) {
            p += 1;
        }
        p
    };
    let quoted = |p: usize| -> Option<usize> {
        let q = at(p);
        if q != b'\'' && q != b'"' {
            return None;
        }
        s[p + 1..]
            .iter()
            .position(|&c| c == q)
            .map(|i| p + 1 + i + 1)
    };
    let mut p = 5;
    if !is_blank(at(p)) {
        return decl_err;
    }
    p = skip(p);
    if !s[p..].starts_with(b"version") {
        return decl_err;
    }
    p = skip(p + 7);
    if at(p) != b'=' {
        return decl_err;
    }
    p = skip(p + 1);
    let Some(np) = quoted(p) else {
        return decl_err;
    };
    p = np;
    let save = p;
    let q = skip(p);
    if s[q..].starts_with(b"encoding") {
        if !is_blank(at(save)) {
            return decl_err;
        }
        p = skip(q + 8);
        if at(p) != b'=' {
            return decl_err;
        }
        p = skip(p + 1);
        let Some(np) = quoted(p) else {
            return decl_err;
        };
        p = np;
    }
    let save = p;
    let q = skip(p);
    if s[q..].starts_with(b"standalone") {
        if !is_blank(at(save)) {
            return decl_err;
        }
        p = skip(q + 10);
        if at(p) != b'=' {
            return decl_err;
        }
        p = skip(p + 1);
        let v = &s[p..];
        if v.starts_with(b"'yes'") || v.starts_with(b"\"yes\"") {
            p += 5;
        } else if v.starts_with(b"'no'") || v.starts_with(b"\"no\"") {
            p += 4;
        } else {
            return decl_err;
        }
    }
    p = skip(p);
    if !s[p..].starts_with(b"?>") {
        return decl_err;
    }
    p += 2;
    if s[..p].iter().any(|&c| c > 127) {
        return decl_err;
    }
    Ok(p)
}

/// Port of xml.c's `xml_doctype_in_content`: a `<!DOCTYPE` after only
/// whitespace, comments and PIs switches PG to document parsing.
fn doctype_in_content(s: &[u8]) -> bool {
    let find = |from: usize, pat: &[u8]| -> Option<usize> {
        s.get(from..)?
            .windows(pat.len())
            .position(|w| w == pat)
            .map(|i| from + i)
    };
    let mut p = 0;
    loop {
        while s.get(p).copied().is_some_and(is_blank) {
            p += 1;
        }
        if s.get(p) != Some(&b'<') {
            return false;
        }
        p += 1;
        if s.get(p) == Some(&b'!') {
            p += 1;
            if s[p..].starts_with(b"DOCTYPE") {
                return true;
            }
            if !s[p..].starts_with(b"--") {
                return false;
            }
            match find(p + 2, b"--") {
                Some(e) if s.get(e + 2) == Some(&b'>') => p = e + 3,
                _ => return false,
            }
            continue;
        }
        if s.get(p) != Some(&b'?') {
            return false;
        }
        match find(p + 1, b"?>") {
            Some(e) => p = e + 2,
            None => return false,
        }
    }
}

struct Parser<'a> {
    s: &'a str,
    p: usize,
    stack: Vec<&'a str>,
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.p..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.p += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, pat: &str) -> bool {
        if self.rest().starts_with(pat) {
            self.p += pat.len();
            true
        } else {
            false
        }
    }

    fn skip_blanks(&mut self) -> bool {
        let start = self.p;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii() && is_blank(c as u8))
        {
            self.p += 1;
        }
        self.p != start
    }

    fn invalid<T>(&self) -> Result<T, Verdict> {
        Err(Verdict::Invalid(CONTENT_ERR))
    }

    fn name(&mut self) -> Result<&'a str, Verdict> {
        let start = self.p;
        match self.peek() {
            Some(c) if is_name_start(c) => {}
            _ => return self.invalid(),
        }
        while self.peek().is_some_and(is_name_char) {
            self.bump();
        }
        Ok(&self.s[start..self.p])
    }

    /// A reference after `&`: `#digits;`, `#xhex;`, or one of the five
    /// predefined entity names followed by `;`.
    fn reference(&mut self) -> Result<(), Verdict> {
        if self.eat("#") {
            let hex = self.eat("x");
            let start = self.p;
            while self.peek().is_some_and(|c| {
                if hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                }
            }) {
                self.p += 1;
            }
            let digits = &self.s[start..self.p];
            if digits.is_empty() || !self.eat(";") {
                return self.invalid();
            }
            let value = if digits.len() > 8 {
                u32::MAX
            } else {
                u32::from_str_radix(digits, if hex { 16 } else { 10 }).unwrap_or(u32::MAX)
            };
            if !is_xml_char(value) {
                return self.invalid();
            }
            return Ok(());
        }
        let name = self.name()?;
        if !self.eat(";") {
            return self.invalid();
        }
        if !matches!(name, "lt" | "gt" | "amp" | "apos" | "quot") {
            return self.invalid();
        }
        Ok(())
    }

    fn check_char(&self, c: char) -> Result<(), Verdict> {
        if is_xml_char(c as u32) {
            Ok(())
        } else {
            self.invalid()
        }
    }

    /// Scan to `terminator`, checking characters; returns false when the
    /// input ends first.
    fn until(&mut self, terminator: &str) -> Result<bool, Verdict> {
        while !self.rest().starts_with(terminator) {
            match self.bump() {
                Some(c) => self.check_char(c)?,
                None => return Ok(false),
            }
        }
        self.p += terminator.len();
        Ok(true)
    }

    fn content(&mut self) -> Result<(), Verdict> {
        while let Some(c) = self.peek() {
            match c {
                '<' => {
                    self.p += 1;
                    self.markup()?;
                }
                '&' => {
                    self.p += 1;
                    self.reference()?;
                }
                _ => {
                    if self.rest().starts_with("]]>") {
                        return self.invalid();
                    }
                    self.check_char(c)?;
                    self.bump();
                }
            }
        }
        if !self.stack.is_empty() {
            return self.invalid();
        }
        Ok(())
    }

    /// After `<`.
    fn markup(&mut self) -> Result<(), Verdict> {
        if self.eat("!--") {
            // `--` may only appear as part of the closing `-->`.
            loop {
                if self.eat("--") {
                    return if self.eat(">") {
                        Ok(())
                    } else {
                        self.invalid()
                    };
                }
                match self.bump() {
                    Some(c) => self.check_char(c)?,
                    None => return self.invalid(),
                }
            }
        }
        if self.eat("![CDATA[") {
            return if self.until("]]>")? {
                Ok(())
            } else {
                self.invalid()
            };
        }
        if self.eat("!") {
            // Not a comment or CDATA section (a DOCTYPE outside the prolog
            // included).
            return self.invalid();
        }
        if self.eat("?") {
            let target = self.name()?;
            if target.eq_ignore_ascii_case("xml") {
                return self.invalid();
            }
            if self.eat("?>") {
                return Ok(());
            }
            if !self.skip_blanks() {
                return self.invalid();
            }
            return if self.until("?>")? {
                Ok(())
            } else {
                self.invalid()
            };
        }
        if self.eat("/") {
            let name = self.name()?;
            self.skip_blanks();
            if !self.eat(">") {
                return self.invalid();
            }
            return match self.stack.pop() {
                Some(open) if open == name => Ok(()),
                _ => self.invalid(),
            };
        }
        // Start tag.
        let name = self.name()?;
        if self.stack.len() > 200 {
            return Err(Verdict::Unsure); // libxml's depth limits
        }
        let mut attrs: Vec<&str> = Vec::new();
        loop {
            let had_blank = self.skip_blanks();
            if self.eat("/>") {
                return Ok(());
            }
            if self.eat(">") {
                self.stack.push(name);
                return Ok(());
            }
            if !had_blank {
                return self.invalid();
            }
            let attr = self.name()?;
            if !attr.contains(':') && attrs.contains(&attr) {
                return self.invalid();
            }
            attrs.push(attr);
            self.skip_blanks();
            if !self.eat("=") {
                return self.invalid();
            }
            self.skip_blanks();
            let quote = match self.bump() {
                Some(q @ ('"' | '\'')) => q,
                _ => return self.invalid(),
            };
            loop {
                match self.peek() {
                    None | Some('<') => return self.invalid(),
                    Some(c) if c == quote => {
                        self.p += 1;
                        break;
                    }
                    Some('&') => {
                        self.p += 1;
                        self.reference()?;
                    }
                    Some(c) => {
                        self.check_char(c)?;
                        self.bump();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn accepted() {
        for ok in [
            "",
            "plain text",
            "a > b",
            "]]",
            "<a></a>",
            "<a/><b/>text",
            "<a x = \"1\" />",
            "<a\n/>",
            "<a x='1'/>",
            "<a x=\"a>b\"/>",
            "<a x=\"&amp;&lt;\"/>",
            "<a:b/>",
            "<:a/>",
            "<a p:x=\"1\" q:x=\"1\"/>",
            "<a.b-c_d/>",
            "<é/>",
            "<a></a >",
            "<!---->",
            "<!-- - -->",
            "<![CDATA[ x < y ]]>",
            "<?pi data?>",
            "<?PI?>",
            "<?pi ?>",
            "<?pi x??>",
            "<?xml-stylesheet a?>",
            "<?xmlx?>",
            "&#65;&#x41;&#9;&#x10FFFF;",
            "<a>&lt;&gt;&quot;&apos;</a>",
            "<?xml version=\"1.0\"?><a/>",
            "<?xml  version=\"1.1\" encoding='x' standalone=\"yes\" ?>",
            "<!DOCTYPE a><a/>",
            "<?xml version=\"1.0\"?><!DOCTYPE a><a/>",
        ] {
            assert!(validate(ok).is_ok(), "{ok:?} should be accepted");
        }
    }

    #[test]
    fn rejected() {
        for bad in [
            "<a>",
            "<a",
            "<",
            "&",
            "</a>",
            "<a></b>",
            "<a></A>",
            "<a><b></a></b>",
            "< a/>",
            "<1a/>",
            "<-a/>",
            "<a b/>",
            "<a x/>",
            "<a x=/>",
            "<a x=1/>",
            "<a x=\"1/>",
            "<a x=\"1\"y=\"2\"/>",
            "<a x=\"1\" x=\"2\"/>",
            "<a x=\"<\"/>",
            "<a x=\"a & b\"/>",
            "<a x=\"&foo;\"/>",
            "<a x=\"&#0;\"/>",
            "<a/ >",
            "a & b",
            "a &foo; b",
            "&amp",
            "&;",
            "&#;",
            "&#x;",
            "&#0;",
            "&#x1;",
            "&#xD800;",
            "&#1114112;",
            "a ]]> b",
            "a\u{1}b",
            "<!-- c -- d -->",
            "<!-- a --->",
            "<!-- x",
            "<![CDATA[x",
            "<!foo>",
            "<a>text<!DOCTYPE a></a>",
            "<? pi?>",
            "<?pi??>",
            "<?pi",
            "<?XmL x?>",
            "<a/><?xml version=\"1.0\"?>",
            " <?xml version=\"1.0\"?>",
        ] {
            assert_eq!(validate(bad).unwrap_err(), "invalid XML content", "{bad:?}");
        }
        for bad in [
            "<?xml?>",
            "<?xml foo?><a/>",
            "<?xml version=1.0?>",
            "<?xml version=\"1.0\"encoding=\"x\"?>",
            "<?xml version=\"1.0\" standalone=\"maybe\"?>",
        ] {
            assert_eq!(
                validate(bad).unwrap_err(),
                "invalid XML content: invalid XML declaration",
                "{bad:?}"
            );
        }
    }
}
