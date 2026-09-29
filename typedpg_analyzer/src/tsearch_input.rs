//! Ports of PostgreSQL 18's `tsvector` and `tsquery` input functions
//! (tsvector_parser.c `gettoken_tsvector`, tsvector.c `tsvectorin`,
//! tsquery.c `gettoken_query_standard` / `parse_phrase_operator` /
//! `get_modifiers` / `makepol` / `pushValue`), used by
//! [`crate::literal_input`].
//!
//! Neither input function consults a text search configuration: `tsqueryin`
//! pushes operands verbatim (`pushval_asis`), so there are no dictionaries
//! or stop words involved. Error messages quote the whole input.

/// `MAXSTRLEN` (tsearch/ts_type.h): the longest lexeme, in bytes.
const MAXSTRLEN: usize = (1 << 11) - 1;
/// `MAXSTRPOS`: the largest lexeme-storage offset.
const MAXSTRPOS: usize = (1 << 20) - 1;
/// `MAXENTRYPOS`: phrase distances are limited to `0..=MAXENTRYPOS`.
const MAXENTRYPOS: i64 = 1 << 14;
/// `STACKDEPTH` of `makepol`'s operator stack.
const STACKDEPTH: usize = 32;

/// C `isspace` on a byte (non-ASCII bytes are never spaces here).
fn c_isspace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// `ISOPERATOR` (tsearch/ts_utils.h).
fn is_operator(b: u8) -> bool {
    matches!(b, b'!' | b'&' | b'|' | b'(' | b')' | b'<')
}

/// Byte length of the UTF-8 character starting with `b`.
fn char_len(b: u8) -> usize {
    match b {
        0xF0.. => 4,
        0xE0.. => 3,
        0xC0.. => 2,
        _ => 1,
    }
}

enum Fail {
    Msg(String),
    /// Beyond what is modelled (e.g. position numbers atoi would
    /// overflow on) — accept.
    Unsure,
}

impl From<String> for Fail {
    fn from(s: String) -> Self {
        Fail::Msg(s)
    }
}

/// `gettoken_tsvector`'s state, shared by both input functions.
struct TsvParser<'a> {
    s: &'a [u8],
    full: &'a str,
    oprisdelim: bool,
    is_tsquery: bool,
}

impl TsvParser<'_> {
    fn at(&self, i: usize) -> u8 {
        self.s.get(i).copied().unwrap_or(0)
    }

    fn syntax(&self) -> Fail {
        Fail::Msg(if self.is_tsquery {
            format!("syntax error in tsquery: \"{}\"", self.full)
        } else {
            format!("syntax error in tsvector: \"{}\"", self.full)
        })
    }

    /// One token from `pos`: `Ok(Some((lexeme_bytes, end)))`, or
    /// `Ok(None)` at end of input.
    fn gettoken(&self, mut pos: usize) -> Result<Option<(usize, usize)>, Fail> {
        #[derive(Clone, Copy, PartialEq)]
        enum St {
            WaitWord,
            WaitEndWord,
            WaitNextChar,
            WaitEndCmplx,
            WaitCharCmplx,
            WaitPosInfo,
            InPosInfo,
            WaitPosDelim,
        }
        let mut state = St::WaitWord;
        let mut old = St::WaitWord;
        let mut len = 0usize; // lexeme bytes so far
        let mut weight_set = false;
        loop {
            let c = self.at(pos);
            let clen = char_len(c);
            match state {
                St::WaitWord => {
                    if c == 0 {
                        return Ok(None);
                    } else if c == b'\'' {
                        state = St::WaitEndCmplx;
                    } else if c == b'\\' {
                        state = St::WaitNextChar;
                        old = St::WaitEndWord;
                    } else if self.oprisdelim && is_operator(c) {
                        return Err(self.syntax());
                    } else if !c_isspace(c) {
                        len += clen;
                        state = St::WaitEndWord;
                    }
                }
                St::WaitNextChar => {
                    if c == 0 {
                        return Err(Fail::Msg(format!(
                            "there is no escaped character: \"{}\"",
                            self.full
                        )));
                    }
                    len += clen;
                    state = old;
                }
                St::WaitEndWord => {
                    if c == b'\\' {
                        state = St::WaitNextChar;
                        old = St::WaitEndWord;
                    } else if c_isspace(c) || c == 0 || (self.oprisdelim && is_operator(c)) {
                        if len == 0 {
                            return Err(self.syntax());
                        }
                        return Ok(Some((len, pos)));
                    } else if c == b':' {
                        if len == 0 {
                            return Err(self.syntax());
                        }
                        if self.oprisdelim {
                            return Ok(Some((len, pos)));
                        }
                        state = St::InPosInfo;
                    } else {
                        len += clen;
                    }
                }
                St::WaitEndCmplx => {
                    if c == b'\'' {
                        state = St::WaitCharCmplx;
                    } else if c == b'\\' {
                        state = St::WaitNextChar;
                        old = St::WaitEndCmplx;
                    } else if c == 0 {
                        return Err(self.syntax());
                    } else {
                        len += clen;
                    }
                }
                St::WaitCharCmplx => {
                    if c == b'\'' {
                        len += 1;
                        state = St::WaitEndCmplx;
                    } else {
                        if len == 0 {
                            return Err(self.syntax());
                        }
                        if self.oprisdelim {
                            return Ok(Some((len, pos)));
                        }
                        state = St::WaitPosInfo;
                        continue; // recheck the current character
                    }
                }
                St::WaitPosInfo => {
                    if c == b':' {
                        state = St::InPosInfo;
                    } else {
                        return Ok(Some((len, pos)));
                    }
                }
                St::InPosInfo => {
                    if !c.is_ascii_digit() {
                        return Err(self.syntax());
                    }
                    let run = self.s[pos..]
                        .iter()
                        .take_while(|b| b.is_ascii_digit())
                        .count();
                    if run > 9 {
                        return Err(Fail::Unsure); // atoi overflow territory
                    }
                    let v: u32 = std::str::from_utf8(&self.s[pos..pos + run])
                        .ok()
                        .and_then(|d| d.parse().ok())
                        .unwrap_or(1);
                    if v == 0 {
                        return Err(Fail::Msg(format!(
                            "wrong position info in tsvector: \"{}\"",
                            self.full
                        )));
                    }
                    weight_set = false;
                    state = St::WaitPosDelim;
                }
                St::WaitPosDelim => {
                    if c == b',' {
                        state = St::InPosInfo;
                    } else if matches!(c, b'a' | b'A' | b'*' | b'b' | b'B' | b'c' | b'C') {
                        if weight_set {
                            return Err(self.syntax());
                        }
                        weight_set = true;
                    } else if matches!(c, b'd' | b'D') {
                        if weight_set {
                            return Err(self.syntax());
                        }
                    } else if c_isspace(c) || c == 0 {
                        return Ok(Some((len, pos)));
                    } else if !c.is_ascii_digit() {
                        return Err(self.syntax());
                    }
                }
            }
            pos += clen;
        }
    }
}

/// Mirrors `tsvectorin`: every token must parse, and each lexeme must fit
/// `MAXSTRLEN` (the running byte total is also checked against
/// `MAXSTRPOS` before each lexeme is added).
pub(crate) fn validate_tsvector(content: &str) -> Result<(), String> {
    let p = TsvParser {
        s: content.as_bytes(),
        full: content,
        oprisdelim: false,
        is_tsquery: false,
    };
    let mut pos = 0;
    let mut total = 0usize;
    loop {
        match p.gettoken(pos) {
            Ok(None) | Err(Fail::Unsure) => return Ok(()),
            Err(Fail::Msg(m)) => return Err(m),
            Ok(Some((len, end))) => {
                if len > MAXSTRLEN {
                    return Err(format!(
                        "word is too long ({len} bytes, max {MAXSTRLEN} bytes)"
                    ));
                }
                if total > MAXSTRPOS {
                    return Err(format!(
                        "string is too long for tsvector ({total} bytes, max {MAXSTRPOS} bytes)"
                    ));
                }
                total += len;
                pos = end;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Not,
    And,
    Or,
    Phrase,
}

impl Op {
    /// `tsearch_op_priority`.
    fn priority(self) -> u8 {
        match self {
            Op::Not => 4,
            Op::Phrase => 3,
            Op::And => 2,
            Op::Or => 1,
        }
    }
}

enum Tok {
    End,
    Err,
    Val(usize),
    Opr(Op),
    Open,
    Close,
}

#[derive(Clone, Copy, PartialEq)]
enum QState {
    /// `WAITFIRSTOPERAND`
    FirstOperand,
    /// `WAITOPERAND`
    Operand,
    /// `WAITOPERATOR`
    Operator,
}

struct QueryParser<'a> {
    tsv: TsvParser<'a>,
    buf: usize,
    count: i64,
    state: QState,
    /// Bytes of operand storage used so far (`curop - op`).
    sumlen: usize,
    depth: usize,
}

impl QueryParser<'_> {
    fn at(&self, i: usize) -> u8 {
        self.tsv.at(i)
    }

    /// `get_modifiers`: `:` then any of `aAbBcCdD*`.
    fn modifiers(&self, mut p: usize) -> usize {
        if self.at(p) != b':' {
            return p;
        }
        p += 1;
        while matches!(
            self.at(p),
            b'a' | b'A' | b'b' | b'B' | b'c' | b'C' | b'd' | b'D' | b'*'
        ) {
            p += 1;
        }
        p
    }

    /// `parse_phrase_operator`: `<->` or `<N>` (0 ≤ N ≤ 16384), which must
    /// not end the input.
    fn phrase_operator(&mut self) -> Result<bool, Fail> {
        let mut p = self.buf;
        if self.at(p) != b'<' {
            return Ok(false);
        }
        p += 1;
        if self.at(p) == b'-' {
            p += 1;
        } else {
            if !self.at(p).is_ascii_digit() {
                return Ok(false);
            }
            let mut v: i64 = 0;
            while self.at(p).is_ascii_digit() {
                v = (v * 10 + i64::from(self.at(p) - b'0')).min(i64::MAX / 20);
                p += 1;
            }
            if v > MAXENTRYPOS {
                return Err(Fail::Msg(format!(
                    "distance in phrase operator must be an integer value between zero and {MAXENTRYPOS} inclusive"
                )));
            }
        }
        if self.at(p) != b'>' {
            return Ok(false);
        }
        p += 1;
        // PHRASE_FINISH is only reached with another character pending.
        if self.at(p) == 0 {
            return Ok(false);
        }
        self.buf = p;
        Ok(true)
    }

    /// `gettoken_query_standard`.
    fn gettoken(&mut self) -> Result<Tok, Fail> {
        loop {
            let c = self.at(self.buf);
            match self.state {
                QState::FirstOperand | QState::Operand => {
                    if c == b'!' {
                        self.buf += 1;
                        self.state = QState::Operand;
                        return Ok(Tok::Opr(Op::Not));
                    } else if c == b'(' {
                        self.buf += 1;
                        self.state = QState::Operand;
                        self.count += 1;
                        return Ok(Tok::Open);
                    } else if c == b':' {
                        return Ok(Tok::Err);
                    } else if !c_isspace(c) {
                        return match self.tsv.gettoken(self.buf)? {
                            Some((len, end)) => {
                                self.buf = self.modifiers(end);
                                self.state = QState::Operator;
                                Ok(Tok::Val(len))
                            }
                            None if self.state == QState::FirstOperand => Ok(Tok::End),
                            None => Err(Fail::Msg(format!(
                                "no operand in tsquery: \"{}\"",
                                self.tsv.full
                            ))),
                        };
                    }
                }
                QState::Operator => {
                    if c == b'&' {
                        self.buf += 1;
                        self.state = QState::Operand;
                        return Ok(Tok::Opr(Op::And));
                    } else if c == b'|' {
                        self.buf += 1;
                        self.state = QState::Operand;
                        return Ok(Tok::Opr(Op::Or));
                    } else if self.phrase_operator()? {
                        self.state = QState::Operand;
                        return Ok(Tok::Opr(Op::Phrase));
                    } else if c == b')' {
                        self.buf += 1;
                        self.count -= 1;
                        return Ok(if self.count < 0 { Tok::Err } else { Tok::Close });
                    } else if c == 0 {
                        return Ok(if self.count != 0 { Tok::Err } else { Tok::End });
                    } else if !c_isspace(c) {
                        return Ok(Tok::Err);
                    }
                }
            }
            self.buf += char_len(c);
        }
    }

    /// `makepol`: one parenthesis level, with its own operator stack.
    fn makepol(&mut self) -> Result<(), Fail> {
        self.depth += 1;
        if self.depth > 1000 {
            return Err(Fail::Unsure); // PG's stack-depth guard territory
        }
        let mut stack: Vec<Op> = Vec::new();
        loop {
            match self.gettoken()? {
                Tok::End | Tok::Close => break,
                Tok::Val(len) => {
                    if len > MAXSTRLEN {
                        return Err(Fail::Msg(format!(
                            "word is too long in tsquery: \"{}\"",
                            self.tsv.full
                        )));
                    }
                    if self.sumlen > MAXSTRPOS {
                        return Err(Fail::Msg(format!(
                            "value is too big in tsquery: \"{}\"",
                            self.tsv.full
                        )));
                    }
                    self.sumlen += len + 1;
                }
                Tok::Opr(op) => {
                    // cleanOpStack: NOT is right-associative.
                    while let Some(&top) = stack.last() {
                        let stop = if op == Op::Not {
                            op.priority() >= top.priority()
                        } else {
                            op.priority() > top.priority()
                        };
                        if stop {
                            break;
                        }
                        stack.pop();
                    }
                    if stack.len() == STACKDEPTH {
                        return Err(Fail::Msg("tsquery stack too small".to_string()));
                    }
                    stack.push(op);
                }
                Tok::Open => self.makepol()?,
                Tok::Err => {
                    return Err(Fail::Msg(format!(
                        "syntax error in tsquery: \"{}\"",
                        self.tsv.full
                    )));
                }
            }
        }
        self.depth -= 1;
        Ok(())
    }
}

/// Mirrors `tsqueryin` (`parse_tsquery` with the standard tokenizer).
pub(crate) fn validate_tsquery(content: &str) -> Result<(), String> {
    let mut q = QueryParser {
        tsv: TsvParser {
            s: content.as_bytes(),
            full: content,
            oprisdelim: true,
            is_tsquery: true,
        },
        buf: 0,
        count: 0,
        state: QState::FirstOperand,
        sumlen: 0,
        depth: 0,
    };
    match q.makepol() {
        Ok(()) | Err(Fail::Unsure) => Ok(()),
        Err(Fail::Msg(m)) => Err(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsquery_inputs() {
        for ok in [
            "",
            "  ",
            "a",
            "a<->b",
            "a <3> b",
            "a:AB*",
            "a:",
            "'a b' & c",
            "(a | b) & !c",
            "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!a",
            "the & a",
            "a & (b | (c <-> d))",
            "é & ü",
        ] {
            assert!(validate_tsquery(ok).is_ok(), "{ok:?} should be valid");
        }
        for (bad, msg) in [
            ("a &", "no operand in tsquery"),
            ("!", "no operand in tsquery"),
            ("a <-> ", "no operand in tsquery"),
            ("()", "syntax error in tsquery"),
            ("& a", "syntax error in tsquery"),
            ("a & & b", "syntax error in tsquery"),
            ("a b", "syntax error in tsquery"),
            ("a <->", "syntax error in tsquery"),
            ("a <-1> b", "syntax error in tsquery"),
            ("a:AB* & b:x", "syntax error in tsquery"),
            (":a", "syntax error in tsquery"),
            ("'a b", "syntax error in tsquery"),
            ("(a | b", "syntax error in tsquery"),
            ("a | b)", "syntax error in tsquery"),
            ("a 'b'", "syntax error in tsquery"),
            ("a\\", "there is no escaped character"),
        ] {
            assert_eq!(
                validate_tsquery(bad).unwrap_err(),
                format!("{msg}: \"{bad}\"")
            );
        }
        assert_eq!(
            validate_tsquery("a <16385> b").unwrap_err(),
            "distance in phrase operator must be an integer value between zero and 16384 inclusive"
        );
        assert_eq!(
            validate_tsquery("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!a").unwrap_err(),
            "tsquery stack too small"
        );
    }

    #[test]
    fn tsvector_inputs() {
        for ok in [
            "", "a:1", "a:1A,2b", "'a b' c", "a & b", ":a", "a:99999", "a:1 , 2", "a:1d,2D",
        ] {
            assert!(validate_tsvector(ok).is_ok(), "{ok:?} should be valid");
        }
        for (bad, msg) in [
            ("a:0", "wrong position info in tsvector"),
            ("a:1AB", "syntax error in tsvector"),
            ("a:x", "syntax error in tsvector"),
            ("a:", "syntax error in tsvector"),
            ("'a b", "syntax error in tsvector"),
            ("a:1,", "syntax error in tsvector"),
            ("a\\", "there is no escaped character"),
        ] {
            assert_eq!(
                validate_tsvector(bad).unwrap_err(),
                format!("{msg}: \"{bad}\"")
            );
        }
        let long = "x".repeat(2048);
        assert_eq!(
            validate_tsvector(&long).unwrap_err(),
            "word is too long (2048 bytes, max 2047 bytes)"
        );
    }
}
