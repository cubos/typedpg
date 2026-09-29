//! Parse-time validation of `jsonpath` literals — a port of PostgreSQL's
//! jsonpath scanner (`jsonpath_scan.l`), grammar (`jsonpath_gram.y`) and
//! the post-parse checks of `flattenJsonPathParseItem` (`jsonpath.c`).
//!
//! `jsonpath_in` runs when an untyped literal is coerced to `jsonpath`
//! (`'$.a'::jsonpath`, `j @? '$.a'`), so a malformed path fails `prepare`.
//! The validator only decides accept / reject, with PG's verbatim message:
//!
//! - scanner errors (`trailing junk after numeric literal`, `invalid
//!   Unicode escape sequence`, …) and grammar errors (`syntax error`) are
//!   rendered like `jsonpath_yyerror`: `<msg> at end of jsonpath input`
//!   when the scanner's current lexeme is empty, else `<msg> at or near
//!   "<lexeme>" of jsonpath input`. The lexeme is flex's `yytext` at the
//!   time the offending token was produced — which for an unquoted word is
//!   its *terminator* (a blank run, or nothing when it is followed by a
//!   special character or the end), and for a quoted string its closing
//!   quote;
//! - `@ is not allowed in root expressions`, `LAST is allowed only in array
//!   subscripts`, the `like_regex` flag checks and `.decimal()`'s argument
//!   count;
//! - the `like_regex` pattern, compiled like `makeItemLikeRegex` does
//!   (`invalid regular expression: …`, see [`crate::regex_input`]).
//!
//! Conservative like the rest of [`crate::literal_input`]: numeric literal
//! magnitudes are not range-checked.

/// Validate `content` as `jsonpath` input. `Err` carries PG's message.
pub(crate) fn validate(content: &str) -> Result<(), String> {
    let toks = Lexer::new(content).run();
    let mut p = Parser {
        toks,
        pos: 0,
        filter_depth: 0,
        in_subscript: 0,
        semantic: None,
    };
    if p.peek().kind == K::LexError {
        return p.error();
    }
    if p.peek().kind == K::Eof {
        return Err(format!(
            "invalid input syntax for type jsonpath: \"{content}\""
        ));
    }
    p.parse_result()?;
    match p.semantic {
        Some(msg) => Err(msg),
        None => Ok(()),
    }
}

/// `makeItemLikeRegex` (jsonpath_gram.y): check the flag characters, map
/// them to `pg_regcomp` flags (`jspConvertRegexFlags`, jsonpath_gram.y) and
/// compile the pattern.
fn like_regex(pattern: &[u8], flags: &[u8]) -> Result<(), String> {
    use crate::regex_input::{REG_ADVANCED, REG_ICASE, REG_NLANCH, REG_NLSTOP, REG_QUOTE};
    let (mut icase, mut dotall, mut mline, mut wspace, mut quote) =
        (false, false, false, false, false);
    for &f in flags {
        match f {
            b'i' => icase = true,
            b's' => dotall = true,
            b'm' => mline = true,
            b'x' => wspace = true,
            b'q' => quote = true,
            _ => return Err("invalid input syntax for type jsonpath".to_string()),
        }
    }
    // XQuery is very nearly Spencer's AREs; `q` (a literal pattern)
    // overrides `x`.
    let mut cflags = REG_ADVANCED;
    if icase {
        cflags |= REG_ICASE;
    }
    if quote {
        cflags &= !REG_ADVANCED;
        cflags |= REG_QUOTE;
    } else {
        if !dotall {
            cflags |= REG_NLSTOP;
        }
        if mline {
            cflags |= REG_NLANCH;
        }
        if wspace {
            return Err(
                "XQuery \"x\" flag (expanded regular expressions) is not implemented".to_string(),
            );
        }
    }
    crate::regex_input::check(&String::from_utf8_lossy(pattern), cflags)
}

fn yyerror(msg: &str, yytext: &str) -> String {
    if yytext.is_empty() {
        format!("{msg} at end of jsonpath input")
    } else {
        format!("{msg} at or near \"{yytext}\" of jsonpath input")
    }
}

// ─── Scanner ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum K {
    Str,
    Var,
    Ident,
    Int,
    Numeric,
    /// A single special character returned as itself.
    Char(u8),
    And,
    Or,
    Not,
    Any,
    Less,
    LessEq,
    Equal,
    NotEqual,
    GreaterEq,
    Greater,
    Kw(Kw),
    Eof,
    /// The scanner failed here; `yytext` holds the complete message. The
    /// scanner runs on demand in PG, so this only surfaces if the parser
    /// needs this token (an earlier syntax error wins).
    LexError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kw {
    Is,
    To,
    Abs,
    Lax,
    Date,
    Flag,
    Last,
    Null,
    Size,
    Time,
    True,
    Type,
    With,
    False,
    Floor,
    Bigint,
    Double,
    Exists,
    Number,
    Starts,
    Strict,
    StringFunc,
    Boolean,
    Ceiling,
    Decimal,
    Integer,
    TimeTz,
    Unknown,
    Datetime,
    Keyvalue,
    Timestamp,
    LikeRegex,
    TimestampTz,
}

/// `keywords[]` of jsonpath_scan.l: `(word, must_be_lowercase, token)`.
/// Matching is case-insensitive except for `null` / `true` / `false`.
const KEYWORDS: &[(&str, bool, Kw)] = &[
    ("is", false, Kw::Is),
    ("to", false, Kw::To),
    ("abs", false, Kw::Abs),
    ("lax", false, Kw::Lax),
    ("date", false, Kw::Date),
    ("flag", false, Kw::Flag),
    ("last", false, Kw::Last),
    ("null", true, Kw::Null),
    ("size", false, Kw::Size),
    ("time", false, Kw::Time),
    ("true", true, Kw::True),
    ("type", false, Kw::Type),
    ("with", false, Kw::With),
    ("false", true, Kw::False),
    ("floor", false, Kw::Floor),
    ("bigint", false, Kw::Bigint),
    ("double", false, Kw::Double),
    ("exists", false, Kw::Exists),
    ("number", false, Kw::Number),
    ("starts", false, Kw::Starts),
    ("strict", false, Kw::Strict),
    ("string", false, Kw::StringFunc),
    ("boolean", false, Kw::Boolean),
    ("ceiling", false, Kw::Ceiling),
    ("decimal", false, Kw::Decimal),
    ("integer", false, Kw::Integer),
    ("time_tz", false, Kw::TimeTz),
    ("unknown", false, Kw::Unknown),
    ("datetime", false, Kw::Datetime),
    ("keyvalue", false, Kw::Keyvalue),
    ("timestamp", false, Kw::Timestamp),
    ("like_regex", false, Kw::LikeRegex),
    ("timestamp_tz", false, Kw::TimestampTz),
];

/// `checkKeyword`: the scanned unquoted word is a keyword or an identifier.
fn check_keyword(word: &[u8]) -> K {
    for &(kw, lowercase, tok) in KEYWORDS {
        if kw.len() == word.len() && kw.as_bytes().eq_ignore_ascii_case(word) {
            if lowercase && kw.as_bytes() != word {
                return K::Ident;
            }
            return K::Kw(tok);
        }
    }
    K::Ident
}

#[derive(Debug, Clone)]
struct Tok {
    kind: K,
    /// flex's `yytext` when this token was returned (see module docs).
    yytext: String,
    /// The token's value: string contents, identifier, number text.
    val: Vec<u8>,
}

/// `special` of jsonpath_scan.l — single-character tokens.
fn is_special(b: u8) -> bool {
    matches!(
        b,
        b'?' | b'%'
            | b'$'
            | b'.'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'('
            | b')'
            | b'|'
            | b'&'
            | b'!'
            | b'='
            | b'<'
            | b'>'
            | b'@'
            | b'#'
            | b','
            | b'*'
            | b':'
            | b'-'
            | b'+'
            | b'/'
    )
}

fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// `other`: anything that is not special, blank, `\` or `"`.
fn is_other(b: u8) -> bool {
    !is_special(b) && !is_blank(b) && b != b'\\' && b != b'"'
}

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
    out: Vec<Tok>,
}

impl<'a> Lexer<'a> {
    fn new(content: &'a str) -> Self {
        Lexer {
            s: content.as_bytes(),
            i: 0,
            out: Vec::new(),
        }
    }

    fn text(&self, from: usize, to: usize) -> String {
        String::from_utf8_lossy(&self.s[from..to]).into_owned()
    }

    fn push(&mut self, kind: K, yytext: String, val: Vec<u8>) {
        self.out.push(Tok { kind, yytext, val });
    }

    fn run(mut self) -> Vec<Tok> {
        if let Err(msg) = self.scan() {
            self.push(K::LexError, msg, Vec::new());
        }
        self.out
    }

    fn scan(&mut self) -> Result<(), String> {
        loop {
            let Some(&b) = self.s.get(self.i) else {
                self.push(K::Eof, String::new(), Vec::new());
                return Ok(());
            };
            let start = self.i;
            let rest = &self.s[self.i..];
            let two = |a: u8, c: u8| rest.len() >= 2 && rest[0] == a && rest[1] == c;

            // Operators (longest match first).
            let op = if two(b'&', b'&') {
                Some((K::And, 2))
            } else if two(b'|', b'|') {
                Some((K::Or, 2))
            } else if two(b'*', b'*') {
                Some((K::Any, 2))
            } else if two(b'<', b'=') {
                Some((K::LessEq, 2))
            } else if two(b'=', b'=') {
                Some((K::Equal, 2))
            } else if two(b'<', b'>') || two(b'!', b'=') {
                Some((K::NotEqual, 2))
            } else if two(b'>', b'=') {
                Some((K::GreaterEq, 2))
            } else if b == b'!' {
                Some((K::Not, 1))
            } else if b == b'<' {
                Some((K::Less, 1))
            } else if b == b'>' {
                Some((K::Greater, 1))
            } else {
                None
            };
            if let Some((kind, len)) = op {
                self.i += len;
                let t = self.text(start, self.i);
                self.push(kind, t, Vec::new());
                continue;
            }

            if b == b'$' {
                // `\${other}+` — a named variable.
                let n = rest[1..].iter().take_while(|&&c| is_other(c)).count();
                if n > 0 {
                    self.i += 1 + n;
                    let t = self.text(start, self.i);
                    let v = self.s[start + 1..self.i].to_vec();
                    self.push(K::Var, t, v);
                    continue;
                }
                if rest.get(1) == Some(&b'"') {
                    // `$"…"` — a quoted variable.
                    self.i += 2;
                    let (v, yytext) = self.quoted()?;
                    self.push(K::Var, yytext, v);
                    continue;
                }
            }

            // Numbers are tried before `{special}` because a leading `.`
            // may start a decimal (`.5`); flex picks the longest match.
            if b.is_ascii_digit() || (b == b'.' && rest.get(1).is_some_and(u8::is_ascii_digit)) {
                let (len, kind) = scan_number(rest);
                // `{other}+` (an identifier) wins when strictly longer.
                let other_len = rest.iter().take_while(|&&c| is_other(c)).count();
                if other_len <= len || !b.is_ascii_digit() {
                    self.i += len;
                    let t = self.text(start, self.i);
                    match kind {
                        NumKind::Int | NumKind::Numeric => {
                            let k = if matches!(kind, NumKind::Int) {
                                K::Int
                            } else {
                                K::Numeric
                            };
                            let v = t.clone().into_bytes();
                            self.push(k, t, v);
                            continue;
                        }
                        NumKind::Junk => {
                            return Err(yyerror("trailing junk after numeric literal", &t));
                        }
                        NumKind::RealFail => {
                            return Err(yyerror("invalid numeric literal", &t));
                        }
                    }
                }
                // Fall through to the identifier rule.
            }

            if is_special(b) {
                self.i += 1;
                if b == b'/' && rest.get(1) == Some(&b'*') {
                    self.i += 1;
                    self.comment()?;
                    continue;
                }
                let t = self.text(start, self.i);
                self.push(K::Char(b), t, Vec::new());
                continue;
            }
            if is_blank(b) {
                self.i += 1;
                continue;
            }
            if b == b'"' {
                self.i += 1;
                let (v, yytext) = self.quoted()?;
                self.push(K::Str, yytext, v);
                continue;
            }
            // `\\` or `{other}+` — an unquoted word (identifier / keyword).
            self.unquoted()?;
        }
    }

    /// `<xc>`: skip a `/* … */` comment (the opener is consumed).
    fn comment(&mut self) -> Result<(), String> {
        while self.i < self.s.len() {
            if self.s[self.i] == b'*' && self.s.get(self.i + 1) == Some(&b'/') {
                self.i += 2;
                return Ok(());
            }
            self.i += 1;
        }
        Err(yyerror("unexpected end of comment", ""))
    }

    /// One escape sequence shared by the `xnq` / `xq` / `xvq` states, with
    /// `self.i` on the backslash. Appends the decoded bytes to `out`.
    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        let s = self.s;
        let start = self.i;
        let Some(&c) = s.get(start + 1) else {
            // `\\` at the very end: "unexpected end after backslash".
            return Err(yyerror("unexpected end after backslash", "\\"));
        };
        match c {
            b'b' | b'f' | b'n' | b'r' | b't' | b'v' => {
                out.push(match c {
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    _ => 0x0b,
                });
                self.i += 2;
            }
            b'u' => {
                // `{unicode}` = \u XXXX | \u{X…} (1–6 hex digits);
                // anything shorter is `{unicodefail}`.
                let hex = |i: usize| s.get(i).is_some_and(u8::is_ascii_hexdigit);
                let (len, value) = if s.get(start + 2) == Some(&b'{') {
                    let n = (0..6).take_while(|&k| hex(start + 3 + k)).count();
                    if n >= 1 && s.get(start + 3 + n) == Some(&b'}') {
                        let v = u32::from_str_radix(
                            std::str::from_utf8(&s[start + 3..start + 3 + n]).unwrap_or("0"),
                            16,
                        )
                        .unwrap_or(0);
                        (4 + n, Some(v))
                    } else {
                        (3 + n, None)
                    }
                } else {
                    let n = (0..4).take_while(|&k| hex(start + 2 + k)).count();
                    if n == 4 {
                        let v = u32::from_str_radix(
                            std::str::from_utf8(&s[start + 2..start + 6]).unwrap_or("0"),
                            16,
                        )
                        .unwrap_or(0);
                        (6, Some(v))
                    } else {
                        (2 + n, None)
                    }
                };
                let Some(v) = value else {
                    let t = self.text(start, start + len);
                    return Err(yyerror("invalid Unicode escape sequence", &t));
                };
                if v == 0 {
                    return Err("unsupported Unicode escape sequence".to_string());
                }
                // Surrogate pairing is not modeled — accept (conservative).
                if let Some(ch) = char::from_u32(v) {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                self.i += len;
            }
            b'x' => {
                let hex = |i: usize| s.get(i).is_some_and(u8::is_ascii_hexdigit);
                if hex(start + 2) && hex(start + 3) {
                    out.push(
                        u8::from_str_radix(
                            std::str::from_utf8(&s[start + 2..start + 4]).unwrap_or("0"),
                            16,
                        )
                        .unwrap_or(0),
                    );
                    self.i += 4;
                } else {
                    let len = if hex(start + 2) { 3 } else { 2 };
                    let t = self.text(start, start + len);
                    return Err(yyerror("invalid hexadecimal character sequence", &t));
                }
            }
            _ => {
                // `\\.` — the escaped character itself (one UTF-8 char).
                let ch_len = utf8_len(c);
                out.extend_from_slice(&s[start + 1..(start + 1 + ch_len).min(s.len())]);
                self.i += 1 + ch_len;
            }
        }
        Ok(())
    }

    /// `<xq>` / `<xvq>` with `self.i` just past the opening quote. Returns
    /// the contents and the token's yytext (the closing quote).
    fn quoted(&mut self) -> Result<(Vec<u8>, String), String> {
        let mut out = Vec::new();
        loop {
            match self.s.get(self.i) {
                None => return Err(yyerror("unterminated quoted string", "")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok((out, "\"".to_string()));
                }
                Some(b'\\') => self.escape(&mut out)?,
                Some(&c) => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// `<xnq>`: an unquoted word, entered on `\\` or `{other}`.
    fn unquoted(&mut self) -> Result<(), String> {
        let mut word = Vec::new();
        loop {
            match self.s.get(self.i) {
                None => {
                    let k = check_keyword(&word);
                    self.push(k, String::new(), word);
                    return Ok(());
                }
                Some(b'\\') => self.escape(&mut word)?,
                Some(&c) if is_other(c) => {
                    word.push(c);
                    self.i += 1;
                }
                Some(&c) if is_blank(c) => {
                    let start = self.i;
                    while self.s.get(self.i).is_some_and(|&c| is_blank(c)) {
                        self.i += 1;
                    }
                    let t = self.text(start, self.i);
                    let k = check_keyword(&word);
                    self.push(k, t, word);
                    return Ok(());
                }
                Some(b'/') if self.s.get(self.i + 1) == Some(&b'*') => {
                    // `<xnq>\/\*` enters the comment state *without*
                    // returning the word, so PG's scanner drops it.
                    self.i += 2;
                    return self.comment();
                }
                Some(_) => {
                    // A special character or `"`: `yyless(0)` leaves an
                    // empty yytext and rescans the terminator.
                    let k = check_keyword(&word);
                    self.push(k, String::new(), word);
                    return Ok(());
                }
            }
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

enum NumKind {
    Int,
    Numeric,
    Junk,
    RealFail,
}

/// Longest numeric-literal match at the start of `s` per jsonpath_scan.l's
/// `decinteger` / `hexinteger` / `decimal` / `real` / `*_junk` /
/// `realfail` rules. Returns `(length, kind)`; the length is never 0 for
/// a digit or `.digit` start.
fn scan_number(s: &[u8]) -> (usize, NumKind) {
    let digits = |from: usize, pred: fn(&u8) -> bool| -> usize {
        // `{d}(_?{d})*` starting at `from`; 0 when no leading digit.
        let mut i = from;
        if !s.get(i).is_some_and(pred) {
            return 0;
        }
        i += 1;
        loop {
            if s.get(i).is_some_and(pred) {
                i += 1;
            } else if s.get(i) == Some(&b'_') && s.get(i + 1).is_some_and(pred) {
                i += 2;
            } else {
                break;
            }
        }
        i - from
    };
    let junk = |len: usize| s.get(len).is_some_and(|&c| is_other(c));

    // Non-decimal integers: 0x / 0o / 0b.
    if s.len() >= 2 && s[0] == b'0' {
        let pred: Option<fn(&u8) -> bool> = match s[1] {
            b'x' | b'X' => Some(u8::is_ascii_hexdigit),
            b'o' | b'O' => Some(|c| (b'0'..=b'7').contains(c)),
            b'b' | b'B' => Some(|c| *c == b'0' || *c == b'1'),
            _ => None,
        };
        if let Some(pred) = pred {
            // No `*_junk` rules exist for the prefixed forms: a trailing
            // `{other}` just makes the identifier rule the longest match.
            let n = digits(2, pred);
            if n > 0 {
                return (2 + n, NumKind::Int);
            }
        }
    }

    // decinteger: 0 | [1-9](_?d)*
    let int_len = if s.first() == Some(&b'0') {
        1
    } else {
        digits(0, u8::is_ascii_digit)
    };
    // decimal: decinteger . decdigits? | . decdigits
    let mut len = int_len;
    let mut is_decimal = false;
    if s.get(len) == Some(&b'.') {
        let frac = digits(len + 1, u8::is_ascii_digit);
        if int_len > 0 || frac > 0 {
            len += 1 + frac;
            is_decimal = true;
        }
    }
    // real: (decinteger|decimal) [Ee] [-+]? decdigits
    if matches!(s.get(len), Some(b'e' | b'E')) {
        let mut j = len + 1;
        let signed = matches!(s.get(j), Some(b'+' | b'-'));
        if signed {
            j += 1;
        }
        let exp = digits(j, u8::is_ascii_digit);
        if exp > 0 {
            let rlen = j + exp;
            return if junk(rlen) {
                (rlen + 1, NumKind::Junk)
            } else {
                (rlen, NumKind::Numeric)
            };
        }
        if signed {
            // realfail — longer than any junk alternative (`1e+`).
            return (j, NumKind::RealFail);
        }
    }
    if junk(len) {
        return (len + 1, NumKind::Junk);
    }
    (
        len,
        if is_decimal {
            NumKind::Numeric
        } else {
            NumKind::Int
        },
    )
}

// ─── Parser ─────────────────────────────────────────────────────────────────

/// What a parsed (sub)expression is, as far as the grammar cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An `expr` (arithmetic / accessor expression).
    Expr,
    /// A `predicate`.
    Pred,
    /// `( predicate )` with no accessor yet: a delimited predicate that could
    /// still become an `accessor_expr` if an accessor follows.
    ParenPred,
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    filter_depth: u32,
    in_subscript: u32,
    /// First post-parse (flatten) error, reported only if parsing succeeds.
    semantic: Option<String>,
}

type PResult<T> = Result<T, String>;

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn kind(&self) -> K {
        self.peek().kind
    }

    fn bump(&mut self) -> Tok {
        let t = self.peek().clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn error<T>(&self) -> PResult<T> {
        let t = self.peek();
        if t.kind == K::LexError {
            return Err(t.yytext.clone());
        }
        Err(yyerror("syntax error", &t.yytext))
    }

    fn expect(&mut self, k: K) -> PResult<Tok> {
        if self.kind() == k {
            Ok(self.bump())
        } else {
            self.error()
        }
    }

    fn semantic(&mut self, msg: &str) {
        self.semantic.get_or_insert_with(|| msg.to_string());
    }

    /// `result: mode expr_or_predicate | EMPTY`
    fn parse_result(&mut self) -> PResult<()> {
        if matches!(self.kind(), K::Kw(Kw::Strict | Kw::Lax)) {
            self.bump();
        }
        self.parse_or()?;
        self.expect(K::Eof)?;
        Ok(())
    }

    /// `predicate OR_P predicate` (lowest precedence), also the entry for
    /// `expr_or_predicate`.
    fn parse_or(&mut self) -> PResult<Kind> {
        let mut left = self.parse_and()?;
        while self.kind() == K::Or {
            if left == Kind::Expr {
                return self.error();
            }
            self.bump();
            let right = self.parse_and()?;
            if right == Kind::Expr {
                return self.error();
            }
            left = Kind::Pred;
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> PResult<Kind> {
        let mut left = self.parse_not()?;
        while self.kind() == K::And {
            if left == Kind::Expr {
                return self.error();
            }
            self.bump();
            let right = self.parse_not()?;
            if right == Kind::Expr {
                return self.error();
            }
            left = Kind::Pred;
        }
        Ok(left)
    }

    /// `NOT_P delimited_predicate` or a comparison-level predicate / expr.
    fn parse_not(&mut self) -> PResult<Kind> {
        if self.kind() != K::Not {
            return self.parse_comparison();
        }
        self.bump();
        match self.kind() {
            K::Char(b'(') => {
                self.bump();
                if self.parse_or()? == Kind::Expr {
                    return self.error();
                }
                self.expect(K::Char(b')'))?;
            }
            K::Kw(Kw::Exists) => self.parse_exists()?,
            _ => return self.error(),
        }
        Ok(Kind::Pred)
    }

    /// `EXISTS_P '(' expr ')'`, with EXISTS as the current token.
    fn parse_exists(&mut self) -> PResult<()> {
        self.bump();
        self.expect(K::Char(b'('))?;
        if self.parse_expr()? != Kind::Expr {
            return self.error();
        }
        self.expect(K::Char(b')'))?;
        Ok(())
    }

    /// `expr comp_op expr`, `expr STARTS WITH …`, `expr LIKE_REGEX …`,
    /// `'(' predicate ')' IS UNKNOWN`, `EXISTS (…)`, or a bare expr.
    fn parse_comparison(&mut self) -> PResult<Kind> {
        if self.kind() == K::Kw(Kw::Exists) {
            self.parse_exists()?;
            return Ok(Kind::Pred);
        }
        let left = self.parse_expr()?;
        if left == Kind::ParenPred {
            if self.kind() == K::Kw(Kw::Is) {
                self.bump();
                self.expect(K::Kw(Kw::Unknown))?;
            }
            return Ok(Kind::Pred);
        }
        if left == Kind::Pred {
            return Ok(Kind::Pred);
        }
        match self.kind() {
            K::Equal | K::NotEqual | K::Less | K::Greater | K::LessEq | K::GreaterEq => {
                self.bump();
                if self.parse_expr()? != Kind::Expr {
                    return self.error();
                }
                Ok(Kind::Pred)
            }
            K::Kw(Kw::Starts) => {
                self.bump();
                self.expect(K::Kw(Kw::With))?;
                match self.kind() {
                    K::Str | K::Var => {
                        self.bump();
                        Ok(Kind::Pred)
                    }
                    _ => self.error(),
                }
            }
            K::Kw(Kw::LikeRegex) => {
                self.bump();
                let pattern = self.expect(K::Str)?;
                let flags = if self.kind() == K::Kw(Kw::Flag) {
                    self.bump();
                    self.expect(K::Str)?.val
                } else {
                    // The rule without FLAG is reduced only once bison has
                    // its lookahead token, so a scanner error there wins.
                    if self.kind() == K::LexError {
                        return self.error();
                    }
                    Vec::new()
                };
                // makeItemLikeRegex runs in the grammar action, so its
                // errors come before any later syntax error.
                like_regex(&pattern.val, &flags)?;
                Ok(Kind::Pred)
            }
            _ => Ok(Kind::Expr),
        }
    }

    /// `expr`: `+`/`-` (binary and unary) over `*`/`/`/`%` over accessor
    /// expressions. Returns `ParenPred` when the whole thing is a lone
    /// parenthesized predicate (which the caller may accept as a predicate).
    fn parse_expr(&mut self) -> PResult<Kind> {
        let mut left = self.parse_term()?;
        while matches!(self.kind(), K::Char(b'+' | b'-')) {
            if left != Kind::Expr {
                return Ok(left);
            }
            self.bump();
            if self.parse_term()? != Kind::Expr {
                return self.error();
            }
            left = Kind::Expr;
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> PResult<Kind> {
        let mut left = self.parse_unary()?;
        while matches!(self.kind(), K::Char(b'*' | b'/' | b'%')) {
            if left != Kind::Expr {
                return Ok(left);
            }
            self.bump();
            if self.parse_unary()? != Kind::Expr {
                return self.error();
            }
            left = Kind::Expr;
        }
        Ok(left)
    }

    /// `'+' expr %prec UMINUS` / `'-' expr %prec UMINUS`.
    fn parse_unary(&mut self) -> PResult<Kind> {
        if matches!(self.kind(), K::Char(b'+' | b'-')) {
            self.bump();
            if self.parse_unary()? != Kind::Expr {
                return self.error();
            }
            return Ok(Kind::Expr);
        }
        self.parse_accessor_expr()
    }

    /// `accessor_expr: path_primary accessor_op* | '(' expr ')'
    /// accessor_op* | '(' predicate ')' accessor_op+`.
    fn parse_accessor_expr(&mut self) -> PResult<Kind> {
        let mut kind = match self.kind() {
            K::Str
            | K::Var
            | K::Int
            | K::Numeric
            | K::Kw(Kw::Null | Kw::True | Kw::False)
            | K::Char(b'$') => {
                self.bump();
                Kind::Expr
            }
            K::Char(b'@') => {
                if self.filter_depth == 0 {
                    self.semantic("@ is not allowed in root expressions");
                }
                self.bump();
                Kind::Expr
            }
            K::Kw(Kw::Last) => {
                if self.in_subscript == 0 {
                    self.semantic("LAST is allowed only in array subscripts");
                }
                self.bump();
                Kind::Expr
            }
            K::Char(b'(') => {
                self.bump();
                let inner = self.parse_or()?;
                self.expect(K::Char(b')'))?;
                if inner == Kind::Expr {
                    Kind::Expr
                } else {
                    Kind::ParenPred
                }
            }
            _ => return self.error(),
        };
        while self.at_accessor_op() {
            self.parse_accessor_op()?;
            kind = Kind::Expr;
        }
        Ok(kind)
    }

    fn at_accessor_op(&self) -> bool {
        matches!(self.kind(), K::Char(b'.' | b'[' | b'?'))
    }

    fn parse_accessor_op(&mut self) -> PResult<()> {
        match self.kind() {
            K::Char(b'[') => {
                self.bump();
                if self.kind() == K::Char(b'*') {
                    self.bump();
                } else {
                    self.in_subscript += 1;
                    loop {
                        if self.parse_expr()? != Kind::Expr {
                            return self.error();
                        }
                        if self.kind() == K::Kw(Kw::To) {
                            self.bump();
                            if self.parse_expr()? != Kind::Expr {
                                return self.error();
                            }
                        }
                        if self.kind() == K::Char(b',') {
                            self.bump();
                            continue;
                        }
                        break;
                    }
                    self.in_subscript -= 1;
                }
                self.expect(K::Char(b']'))?;
            }
            K::Char(b'?') => {
                self.bump();
                self.expect(K::Char(b'('))?;
                self.filter_depth += 1;
                if self.parse_or()? == Kind::Expr {
                    return self.error();
                }
                self.filter_depth -= 1;
                self.expect(K::Char(b')'))?;
            }
            _ => {
                // '.'
                self.bump();
                match self.kind() {
                    K::Char(b'*') => {
                        self.bump();
                    }
                    K::Any => {
                        self.bump();
                        if self.kind() == K::Char(b'{') {
                            self.bump();
                            self.parse_any_level()?;
                            if self.kind() == K::Kw(Kw::To) {
                                self.bump();
                                self.parse_any_level()?;
                            }
                            self.expect(K::Char(b'}'))?;
                        }
                    }
                    K::Ident | K::Str => {
                        self.bump();
                    }
                    K::Kw(kw) => {
                        self.bump();
                        if self.kind() == K::Char(b'(') {
                            self.parse_method_args(kw)?;
                        }
                    }
                    _ => return self.error(),
                }
            }
        }
        Ok(())
    }

    /// `any_level: INT_P | LAST_P`
    fn parse_any_level(&mut self) -> PResult<()> {
        match self.kind() {
            K::Int | K::Kw(Kw::Last) => {
                self.bump();
                Ok(())
            }
            _ => self.error(),
        }
    }

    /// `'.' method '(' … ')'` with the method keyword consumed and `(` next.
    /// A keyword that is not a method is only a key, so `(` is an error.
    fn parse_method_args(&mut self, kw: Kw) -> PResult<()> {
        match kw {
            Kw::Abs
            | Kw::Size
            | Kw::Type
            | Kw::Floor
            | Kw::Double
            | Kw::Ceiling
            | Kw::Keyvalue
            | Kw::Bigint
            | Kw::Boolean
            | Kw::Date
            | Kw::Integer
            | Kw::Number
            | Kw::StringFunc => {
                self.bump();
                self.expect(K::Char(b')'))?;
            }
            Kw::Datetime => {
                self.bump();
                if self.kind() == K::Str {
                    self.bump();
                }
                self.expect(K::Char(b')'))?;
            }
            Kw::Time | Kw::TimeTz | Kw::Timestamp | Kw::TimestampTz => {
                self.bump();
                if self.kind() != K::Char(b')') {
                    self.parse_int_elem()?;
                }
                self.expect(K::Char(b')'))?;
            }
            Kw::Decimal => {
                self.bump();
                let mut n = 0;
                if self.kind() != K::Char(b')') {
                    loop {
                        self.parse_int_elem()?;
                        n += 1;
                        if self.kind() == K::Char(b',') {
                            self.bump();
                            continue;
                        }
                        break;
                    }
                }
                self.expect(K::Char(b')'))?;
                if n > 2 {
                    return Err("invalid input syntax for type jsonpath".to_string());
                }
            }
            _ => return self.error(),
        }
        Ok(())
    }

    /// `int_elem: INT_P | '+' INT_P | '-' INT_P`
    fn parse_int_elem(&mut self) -> PResult<()> {
        if matches!(self.kind(), K::Char(b'+' | b'-')) {
            self.bump();
        }
        self.expect(K::Int)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn accepts_valid_paths() {
        for p in [
            "$",
            "strict $.a[*]",
            "LAX $",
            "$.a ? (@.b == 1 && @.c < 2 || !(@.d > 3))",
            "$[last]",
            "$[1, 2 to last]",
            "$.**{1 to last}",
            "$.datetime(\"yyyy\")",
            "$.decimal(4, 2)",
            "$ ? (@ starts with $x)",
            "$ like_regex \"a\" flag \"iq\"",
            "$ like_regex \"(\" flag \"xq\"",
            "$ like_regex \"^(a|b)+\\\\d{2}$\" flag \"sm\"",
            "$.a`b",
            "$.\"a b\"",
            "true.a",
            "-$.a * 2 + 1",
            "$ /* comment */",
            "$ ? ((@.a == 1).type() == \"boolean\")",
            "$.Size()",
        ] {
            let r = validate(p);
            assert!(
                r.is_ok() || r.as_ref().is_err_and(|e| e.contains("@ is not allowed")),
                "{p}: {r:?}"
            );
        }
    }

    #[test]
    fn rejects_like_pg() {
        for (p, msg) in [
            ("x", "syntax error at end of jsonpath input"),
            (
                "$.a.bad(",
                "syntax error at or near \"(\" of jsonpath input",
            ),
            (
                "$ \"x\"",
                "syntax error at or near \"\"\" of jsonpath input",
            ),
            ("$.1", "syntax error at or near \".1\" of jsonpath input"),
            ("-(1 == 1)", "syntax error at end of jsonpath input"),
            ("!(1)", "syntax error at or near \")\" of jsonpath input"),
            ("@", "@ is not allowed in root expressions"),
            ("last", "LAST is allowed only in array subscripts"),
            (
                "1a",
                "trailing junk after numeric literal at or near \"1a\" of jsonpath input",
            ),
            (
                "$ /* c",
                "unexpected end of comment at end of jsonpath input",
            ),
            (
                "\"abc",
                "unterminated quoted string at end of jsonpath input",
            ),
            (
                "$ ? (@.x is unknown)",
                "syntax error at or near \" \" of jsonpath input",
            ),
            ("", "invalid input syntax for type jsonpath: \"\""),
            (
                "$ like_regex \"(\"",
                "invalid regular expression: parentheses () not balanced",
            ),
            (
                "$ like_regex \"a\" flag \"xz\"",
                "invalid input syntax for type jsonpath",
            ),
            (
                "$ like_regex \"(\" flag \"x\"",
                "XQuery \"x\" flag (expanded regular expressions) is not implemented",
            ),
        ] {
            assert_eq!(validate(p), Err(msg.to_string()), "{p}");
        }
    }
}
