//! Compile-time validation of PostgreSQL regular expressions — a port of
//! the *syntax* half of Henry Spencer's regex compiler as shipped in
//! PostgreSQL (`src/backend/regex/regcomp.c`, `regc_lex.c`,
//! `regc_locale.c`).
//!
//! [`check`] answers one question: would `pg_regcomp(pattern, cflags)`
//! fail, and with which `regerror` text? It walks the same lexer states
//! (`next`, `lexescape`, `brenext`, `prefixes`, `skip`) and the same
//! recursive-descent parser (`parse` / `parsebranch` / `parseqatom` /
//! `bracket` / `brackpart` / `scanplain` / `scannum`) as PG, calling the
//! lexer at exactly the points PG does, so the *first* error recorded
//! (PG's `ERR` keeps the first one) is the one reported. NFA construction
//! is not modelled: it can only fail with `REG_ETOOBIG` / `REG_ESPACE`
//! (size limits), which are accepted unchecked.
//!
//! Modelled: AREs, EREs and BREs (reachable via the `(?e)` / `(?b)`
//! embedded options), the `***=` / `***:` / `***?` director prefixes,
//! embedded options, `REG_QUOTE` literals, expanded mode (`(?x)`)
//! whitespace/comment skipping, `(?#…)` comments, non-capturing groups,
//! lookahead/lookbehind constraints (backrefs inside them are
//! `REG_ESUBREG`), bounds with `DUPMAX` = 255, bracket expressions
//! (`[:class:]`, `[.coll.]`, `[=equiv=]`, ranges, escapes), and every ARE
//! escape (`\d \w \s \m \M \y \Y \Z \A`, `\x \u \U \0 \c`, backrefs and
//! their octal fallback).
//!
//! Accepted unchecked (never rejected): locale-dependent classification of
//! non-ASCII characters where it can change the outcome — a non-ASCII
//! white-space character skipped in expanded mode, a non-ASCII digit after
//! `{` — plus the size limits above and nesting deeper than
//! [`MAX_DEPTH`].

/// `regex.h` compile flags (same values as PG).
pub(crate) const REG_EXTENDED: u32 = 0o1;
pub(crate) const REG_ADVF: u32 = 0o2;
pub(crate) const REG_ADVANCED: u32 = 0o3;
pub(crate) const REG_QUOTE: u32 = 0o4;
pub(crate) const REG_ICASE: u32 = 0o10;
pub(crate) const REG_EXPANDED: u32 = 0o40;
pub(crate) const REG_NLSTOP: u32 = 0o100;
pub(crate) const REG_NLANCH: u32 = 0o200;
pub(crate) const REG_NEWLINE: u32 = 0o300;

/// `DUPMAX` (`_POSIX2_RE_DUP_MAX`) and `DUPINF` of regguts.h.
const DUPMAX: u32 = 255;
const DUPINF: u32 = DUPMAX + 1;
/// `CHR_MAX` of regcustom.h (`CHR_IS_IN_RANGE`).
const CHR_MAX: u32 = 0x7fff_fffe;
/// Parenthesis nesting beyond which we stop checking (PG has no syntax
/// limit, but a deep pattern would recurse here without bound).
const MAX_DEPTH: u32 = 256;

/// The `REG_*` error codes `pg_regcomp`'s parser can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegError {
    BadPat,
    ECollate,
    ECType,
    EEscape,
    ESubReg,
    EBrack,
    EParen,
    EBrace,
    BadBr,
    ERange,
    BadRpt,
    BadOpt,
}

impl RegError {
    /// The `regerror` text (regex/regerrs.h).
    pub(crate) fn text(self) -> &'static str {
        match self {
            RegError::BadPat => "invalid regexp (reg version 0.8)",
            RegError::ECollate => "invalid collating element",
            RegError::ECType => "invalid character class",
            RegError::EEscape => "invalid escape \\ sequence",
            RegError::ESubReg => "invalid backreference number",
            RegError::EBrack => "brackets [] not balanced",
            RegError::EParen => "parentheses () not balanced",
            RegError::EBrace => "braces {} not balanced",
            RegError::BadBr => "invalid repetition count(s)",
            RegError::ERange => "invalid character range",
            RegError::BadRpt => "quantifier operand invalid",
            RegError::BadOpt => "invalid embedded option",
        }
    }
}

/// Validate `pattern` as `pg_regcomp` would with `cflags`. `Err` carries
/// PG's message, `invalid regular expression: <regerror text>` (the
/// wording of every backend caller, e.g. `RE_compile_and_cache` and
/// jsonpath's `makeItemLikeRegex`).
pub(crate) fn check(pattern: &str, cflags: u32) -> Result<(), String> {
    match compile(pattern, cflags) {
        Some(e) => Err(format!("invalid regular expression: {}", e.text())),
        None => Ok(()),
    }
}

/// The `REG_*` error `pg_regcomp` reports for `pattern`, or `None` when it
/// compiles (or when we can't tell — see the module docs).
pub(crate) fn compile(pattern: &str, cflags: u32) -> Option<RegError> {
    let mut v = V {
        s: pattern.chars().map(|c| c as u32).collect(),
        now: 0,
        cflags,
        lexcon: Con::Ere,
        nexttype: T::Empty,
        nextvalue: 0,
        lasttype: T::Empty,
        nsubexp: 0,
        nsubs: 10,
        closed: Vec::new(),
        depth: 0,
    };
    match v.run() {
        Ok(()) | Err(Stop::Unknown) => None,
        Err(Stop::Err(e)) => Some(e),
    }
}

/// Why checking stopped early.
enum Stop {
    /// `pg_regcomp` fails with this code.
    Err(RegError),
    /// Outcome depends on something we don't model — accept.
    Unknown,
}

impl From<RegError> for Stop {
    fn from(e: RegError) -> Self {
        Stop::Err(e)
    }
}

type R<T = ()> = Result<T, Stop>;

/// Token types (`nexttype`) of regcomp.c.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum T {
    Empty,
    Eos,
    Plain,
    Digit,
    Comma,
    LBrace,
    RBrace,
    Pipe,
    Star,
    Plus,
    Quest,
    LParen,
    RParen,
    LBrack,
    RBrack,
    Dot,
    Caret,
    Dollar,
    /// `<` / `\m` / `[[:<:]]`
    Lt,
    /// `>` / `\M` / `[[:>:]]`
    Gt,
    Backref,
    SBegin,
    SEnd,
    WBdry,
    NWBdry,
    Lacon,
    CClassS,
    CClassC,
    Range,
    CollEl,
    EClass,
    CClass,
    End,
}

/// Lexical contexts (`L_*` of regc_lex.c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Con {
    Ere,
    Bre,
    Q,
    EBnd,
    BBnd,
    Brack,
    Cel,
    Ecl,
    Ccl,
}

/// `struct vars`, reduced to what the syntax checks read.
struct V {
    s: Vec<u32>,
    now: usize,
    cflags: u32,
    lexcon: Con,
    nexttype: T,
    nextvalue: u32,
    lasttype: T,
    /// Capturing groups opened so far (`v->nsubexp`).
    nsubexp: u32,
    /// Size of `v->subs` (grown by `moresubs`).
    nsubs: u32,
    /// `v->subs[n] != NULL`: group `n` has been completely parsed.
    closed: Vec<bool>,
    depth: u32,
}

const fn ch(c: char) -> u32 {
    c as u32
}

/// `iscspace` for ASCII (regc_pg_locale.c's table); non-ASCII white space
/// is locale-dependent.
fn space_class(c: u32) -> Option<bool> {
    if c < 128 {
        return Some(matches!(c, 0x20 | 0x09..=0x0d));
    }
    let ch = char::from_u32(c)?;
    if ch.is_whitespace() || matches!(c, 0x180e | 0x200b | 0xfeff) {
        None
    } else {
        Some(false)
    }
}

/// `iscdigit` as the libc provider answers it: ASCII digits only.
///
/// Its one use is `{` followed by a digit starting a bound. ICU and the
/// builtin Unicode provider also count other Unicode digits, but for those
/// the bound lexer then fails at once (it only takes ASCII digits:
/// `REG_BADBR`), so the libc reading — `{` is a plain character — never
/// rejects a pattern another provider accepts.
fn is_digit(c: u32) -> bool {
    (ch('0')..=ch('9')).contains(&c)
}

/// `iscalpha` (only used to decide between two error codes, so an
/// approximation for non-ASCII is fine).
fn is_alpha(c: u32) -> bool {
    char::from_u32(c).is_some_and(|c| c.is_alphabetic())
}

/// ASCII names of `[[.name.]]` collating elements (regc_locale.c
/// `cnames`), with their codes.
const CNAMES: &[(&str, u8)] = &[
    ("NUL", 0),
    ("SOH", 1),
    ("STX", 2),
    ("ETX", 3),
    ("EOT", 4),
    ("ENQ", 5),
    ("ACK", 6),
    ("BEL", 7),
    ("alert", 7),
    ("BS", 8),
    ("backspace", 8),
    ("HT", 9),
    ("tab", 9),
    ("LF", 10),
    ("newline", 10),
    ("VT", 11),
    ("vertical-tab", 11),
    ("FF", 12),
    ("form-feed", 12),
    ("CR", 13),
    ("carriage-return", 13),
    ("SO", 14),
    ("SI", 15),
    ("DLE", 16),
    ("DC1", 17),
    ("DC2", 18),
    ("DC3", 19),
    ("DC4", 20),
    ("NAK", 21),
    ("SYN", 22),
    ("ETB", 23),
    ("CAN", 24),
    ("EM", 25),
    ("SUB", 26),
    ("ESC", 27),
    ("IS4", 28),
    ("FS", 28),
    ("IS3", 29),
    ("GS", 29),
    ("IS2", 30),
    ("RS", 30),
    ("IS1", 31),
    ("US", 31),
    ("space", b' '),
    ("exclamation-mark", b'!'),
    ("quotation-mark", b'"'),
    ("number-sign", b'#'),
    ("dollar-sign", b'$'),
    ("percent-sign", b'%'),
    ("ampersand", b'&'),
    ("apostrophe", b'\''),
    ("left-parenthesis", b'('),
    ("right-parenthesis", b')'),
    ("asterisk", b'*'),
    ("plus-sign", b'+'),
    ("comma", b','),
    ("hyphen", b'-'),
    ("hyphen-minus", b'-'),
    ("period", b'.'),
    ("full-stop", b'.'),
    ("slash", b'/'),
    ("solidus", b'/'),
    ("zero", b'0'),
    ("one", b'1'),
    ("two", b'2'),
    ("three", b'3'),
    ("four", b'4'),
    ("five", b'5'),
    ("six", b'6'),
    ("seven", b'7'),
    ("eight", b'8'),
    ("nine", b'9'),
    ("colon", b':'),
    ("semicolon", b';'),
    ("less-than-sign", b'<'),
    ("equals-sign", b'='),
    ("greater-than-sign", b'>'),
    ("question-mark", b'?'),
    ("commercial-at", b'@'),
    ("left-square-bracket", b'['),
    ("backslash", b'\\'),
    ("reverse-solidus", b'\\'),
    ("right-square-bracket", b']'),
    ("circumflex", b'^'),
    ("circumflex-accent", b'^'),
    ("underscore", b'_'),
    ("low-line", b'_'),
    ("grave-accent", b'`'),
    ("left-brace", b'{'),
    ("left-curly-bracket", b'{'),
    ("vertical-line", b'|'),
    ("right-brace", b'}'),
    ("right-curly-bracket", b'}'),
    ("tilde", b'~'),
    ("DEL", 0o177),
];

/// regc_locale.c `classNames`.
const CLASS_NAMES: &[&str] = &[
    "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct",
    "space", "upper", "xdigit", "word",
];

fn name_eq(name: &str, s: &[u32]) -> bool {
    name.len() == s.len() && name.bytes().zip(s).all(|(b, &c)| b as u32 == c)
}

impl V {
    // ─── scanning macros ────────────────────────────────────────────────

    fn ateos(&self) -> bool {
        self.now >= self.s.len()
    }

    fn have(&self, n: usize) -> bool {
        self.s.len() - self.now >= n
    }

    fn next1(&self, c: char) -> bool {
        !self.ateos() && self.s[self.now] == ch(c)
    }

    fn next2(&self, a: char, b: char) -> bool {
        self.have(2) && self.s[self.now] == ch(a) && self.s[self.now + 1] == ch(b)
    }

    fn see(&self, t: T) -> bool {
        self.nexttype == t
    }

    fn ret(&mut self, t: T) -> R {
        self.nexttype = t;
        Ok(())
    }

    fn retv(&mut self, t: T, val: u32) -> R {
        self.nexttype = t;
        self.nextvalue = val;
        Ok(())
    }

    fn advf(&self) -> bool {
        self.cflags & REG_ADVF != 0
    }

    // ─── pg_regcomp ─────────────────────────────────────────────────────

    fn run(&mut self) -> R {
        self.lexstart()?;
        if self.lexcon == Con::Q {
            // A literal string is a run of PLAIN tokens: it always compiles.
            return Ok(());
        }
        self.parse(T::Eos, false)
    }

    // ─── regc_lex.c ─────────────────────────────────────────────────────

    /// `lexstart`
    fn lexstart(&mut self) -> R {
        self.prefixes()?;
        self.lexcon = if self.cflags & REG_QUOTE != 0 {
            Con::Q
        } else if self.cflags & REG_EXTENDED != 0 {
            Con::Ere
        } else {
            Con::Bre
        };
        self.nexttype = T::Empty;
        self.next()
    }

    /// `prefixes` — `***` directors and ARE embedded options.
    fn prefixes(&mut self) -> R {
        if self.cflags & REG_QUOTE != 0 {
            return Ok(());
        }
        if self.have(4) && self.s[self.now..self.now + 3] == [ch('*'); 3] {
            match char::from_u32(self.s[self.now + 3]) {
                Some('?') => return Err(RegError::BadPat.into()),
                Some('=') => {
                    self.cflags |= REG_QUOTE;
                    self.cflags &= !(REG_ADVANCED | REG_EXPANDED | REG_NEWLINE);
                    self.now += 4;
                    return Ok(());
                }
                Some(':') => {
                    self.cflags |= REG_ADVANCED;
                    self.now += 4;
                }
                _ => return Err(RegError::BadRpt.into()),
            }
        }
        if self.cflags & REG_ADVANCED != REG_ADVANCED {
            return Ok(());
        }
        if self.have(3) && self.next2('(', '?') && is_alpha(self.s[self.now + 2]) {
            self.now += 2;
            while !self.ateos() && is_alpha(self.s[self.now]) {
                match char::from_u32(self.s[self.now]) {
                    Some('b') => self.cflags &= !(REG_ADVANCED | REG_QUOTE),
                    Some('c') => self.cflags &= !REG_ICASE,
                    Some('e') => {
                        self.cflags |= REG_EXTENDED;
                        self.cflags &= !(REG_ADVF | REG_QUOTE);
                    }
                    Some('i') => self.cflags |= REG_ICASE,
                    Some('m' | 'n') => self.cflags |= REG_NEWLINE,
                    Some('p') => {
                        self.cflags |= REG_NLSTOP;
                        self.cflags &= !REG_NLANCH;
                    }
                    Some('q') => {
                        self.cflags |= REG_QUOTE;
                        self.cflags &= !REG_ADVANCED;
                    }
                    Some('s') => self.cflags &= !REG_NEWLINE,
                    Some('t') => self.cflags &= !REG_EXPANDED,
                    Some('w') => {
                        self.cflags &= !REG_NLSTOP;
                        self.cflags |= REG_NLANCH;
                    }
                    Some('x') => self.cflags |= REG_EXPANDED,
                    _ => return Err(RegError::BadOpt.into()),
                }
                self.now += 1;
            }
            if !self.next1(')') {
                return Err(RegError::BadOpt.into());
            }
            self.now += 1;
            if self.cflags & REG_QUOTE != 0 {
                self.cflags &= !(REG_EXPANDED | REG_NEWLINE);
            }
        }
        Ok(())
    }

    /// `next` — get the next token.
    fn next(&mut self) -> R {
        loop {
            self.lasttype = self.nexttype;

            if self.cflags & REG_EXPANDED != 0
                && matches!(self.lexcon, Con::Ere | Con::Bre | Con::EBnd | Con::BBnd)
            {
                self.skip()?;
            }

            if self.ateos() {
                return match self.lexcon {
                    Con::Ere | Con::Bre | Con::Q => self.ret(T::Eos),
                    Con::EBnd | Con::BBnd => Err(RegError::EBrace.into()),
                    Con::Brack | Con::Cel | Con::Ecl | Con::Ccl => Err(RegError::EBrack.into()),
                };
            }

            let c = self.s[self.now];
            self.now += 1;
            let cc = char::from_u32(c).unwrap_or('\0');

            match self.lexcon {
                Con::Bre => return self.brenext(c),
                Con::Ere => {}
                Con::Q => return self.retv(T::Plain, c),
                Con::EBnd | Con::BBnd => {
                    return match cc {
                        '0'..='9' => self.retv(T::Digit, c - ch('0')),
                        ',' => self.ret(T::Comma),
                        '}' if self.lexcon == Con::EBnd => {
                            self.lexcon = Con::Ere;
                            if self.advf() && self.next1('?') {
                                self.now += 1;
                                return self.retv(T::RBrace, 0);
                            }
                            self.retv(T::RBrace, 1)
                        }
                        '\\' if self.lexcon == Con::BBnd && self.next1('}') => {
                            self.now += 1;
                            self.lexcon = Con::Bre;
                            self.retv(T::RBrace, 1)
                        }
                        _ => Err(RegError::BadBr.into()),
                    };
                }
                Con::Brack => return self.brack_next(c),
                Con::Cel | Con::Ecl | Con::Ccl => {
                    let close = match self.lexcon {
                        Con::Cel => '.',
                        Con::Ecl => '=',
                        _ => ':',
                    };
                    if cc == close && self.next1(']') {
                        self.now += 1;
                        self.lexcon = Con::Brack;
                        return self.retv(T::End, c);
                    }
                    return self.retv(T::Plain, c);
                }
            }

            // EREs and AREs.
            match cc {
                '|' => return self.ret(T::Pipe),
                '*' | '+' | '?' => {
                    let t = match cc {
                        '*' => T::Star,
                        '+' => T::Plus,
                        _ => T::Quest,
                    };
                    if self.advf() && self.next1('?') {
                        self.now += 1;
                        return self.retv(t, 0);
                    }
                    return self.retv(t, 1);
                }
                '{' => {
                    if self.cflags & REG_EXPANDED != 0 {
                        self.skip()?;
                    }
                    if self.ateos() || !is_digit(self.s[self.now]) {
                        return self.retv(T::Plain, c);
                    }
                    self.lexcon = Con::EBnd;
                    return self.ret(T::LBrace);
                }
                '(' => {
                    if self.advf() && self.next1('?') {
                        self.now += 1;
                        if self.ateos() {
                            return Err(RegError::BadRpt.into());
                        }
                        let k = char::from_u32(self.s[self.now]);
                        self.now += 1;
                        match k {
                            Some(':') => return self.retv(T::LParen, 0),
                            Some('#') => {
                                while !self.ateos() && self.s[self.now] != ch(')') {
                                    self.now += 1;
                                }
                                if !self.ateos() {
                                    self.now += 1;
                                }
                                continue; // next_restart
                            }
                            Some('=' | '!') => return self.ret(T::Lacon),
                            Some('<') => {
                                if self.ateos() {
                                    return Err(RegError::BadRpt.into());
                                }
                                let k2 = self.s[self.now];
                                self.now += 1;
                                if k2 == ch('=') || k2 == ch('!') {
                                    return self.ret(T::Lacon);
                                }
                                return Err(RegError::BadRpt.into());
                            }
                            _ => return Err(RegError::BadRpt.into()),
                        }
                    }
                    return self.retv(T::LParen, 1);
                }
                ')' => return self.retv(T::RParen, c),
                '[' => return self.open_bracket(),
                '.' => return self.ret(T::Dot),
                '^' => return self.ret(T::Caret),
                '$' => return self.ret(T::Dollar),
                '\\' => {
                    if self.ateos() {
                        return Err(RegError::EEscape.into());
                    }
                }
                _ => return self.retv(T::Plain, c),
            }

            // ERE/ARE backslash, backslash already eaten.
            if !self.advf() {
                let c = self.s[self.now];
                self.now += 1;
                return self.retv(T::Plain, c);
            }
            return self.lexescape();
        }
    }

    /// A mainline `[`: `[[:<:]]` / `[[:>:]]` word constraints, or the start
    /// of a bracket expression (shared by `next` and `brenext`).
    fn open_bracket(&mut self) -> R {
        if self.have(6)
            && self.s[self.now] == ch('[')
            && self.s[self.now + 1] == ch(':')
            && (self.s[self.now + 2] == ch('<') || self.s[self.now + 2] == ch('>'))
            && self.s[self.now + 3] == ch(':')
            && self.s[self.now + 4] == ch(']')
            && self.s[self.now + 5] == ch(']')
        {
            let lt = self.s[self.now + 2] == ch('<');
            self.now += 6;
            return self.ret(if lt { T::Lt } else { T::Gt });
        }
        self.lexcon = Con::Brack;
        if self.next1('^') {
            self.now += 1;
            return self.retv(T::LBrack, 0);
        }
        self.retv(T::LBrack, 1)
    }

    /// The `L_BRACK` arm of `next`.
    fn brack_next(&mut self, c: u32) -> R {
        match char::from_u32(c).unwrap_or('\0') {
            ']' => {
                if self.lasttype == T::LBrack {
                    return self.retv(T::Plain, c);
                }
                self.lexcon = if self.cflags & REG_EXTENDED != 0 {
                    Con::Ere
                } else {
                    Con::Bre
                };
                self.ret(T::RBrack)
            }
            '\\' => {
                if !self.advf() {
                    return self.retv(T::Plain, c);
                }
                if self.ateos() {
                    return Err(RegError::EEscape.into());
                }
                self.lexescape()?;
                match self.nexttype {
                    T::Plain | T::CClassS | T::CClassC => Ok(()),
                    _ => Err(RegError::EEscape.into()),
                }
            }
            '-' => {
                if self.lasttype == T::LBrack || self.next1(']') {
                    self.retv(T::Plain, c)
                } else {
                    self.retv(T::Range, c)
                }
            }
            '[' => {
                if self.ateos() {
                    return Err(RegError::EBrack.into());
                }
                let k = self.s[self.now];
                self.now += 1;
                match char::from_u32(k) {
                    Some('.') => {
                        self.lexcon = Con::Cel;
                        self.ret(T::CollEl)
                    }
                    Some('=') => {
                        self.lexcon = Con::Ecl;
                        self.ret(T::EClass)
                    }
                    Some(':') => {
                        self.lexcon = Con::Ccl;
                        self.ret(T::CClass)
                    }
                    _ => {
                        self.now -= 1;
                        self.retv(T::Plain, c)
                    }
                }
            }
            _ => self.retv(T::Plain, c),
        }
    }

    /// `lexescape` — an ARE backslash escape (backslash already eaten).
    fn lexescape(&mut self) -> R {
        let c = self.s[self.now];
        self.now += 1;
        let cc = char::from_u32(c).unwrap_or('\0');
        if !cc.is_ascii_alphanumeric() {
            return self.retv(T::Plain, c);
        }
        match cc {
            'a' => self.retv(T::Plain, 0o7),
            'A' => self.ret(T::SBegin),
            'b' => self.retv(T::Plain, 0o10),
            'B' => self.retv(T::Plain, ch('\\')),
            'c' => {
                if self.ateos() {
                    return Err(RegError::EEscape.into());
                }
                let n = self.s[self.now] & 0o37;
                self.now += 1;
                self.retv(T::Plain, n)
            }
            'd' | 's' | 'w' => self.ret(T::CClassS),
            'D' | 'S' | 'W' => self.ret(T::CClassC),
            'e' => self.retv(T::Plain, 0o33),
            'f' => self.retv(T::Plain, 0o14),
            'm' => self.ret(T::Lt),
            'M' => self.ret(T::Gt),
            'n' => self.retv(T::Plain, ch('\n')),
            'r' => self.retv(T::Plain, ch('\r')),
            't' => self.retv(T::Plain, ch('\t')),
            'v' => self.retv(T::Plain, 0o13),
            'u' | 'U' | 'x' => {
                let (min, max) = match cc {
                    'u' => (4, 4),
                    'U' => (8, 8),
                    _ => (1, 255),
                };
                let n = self.lexdigits(16, min, max)?;
                if n > CHR_MAX {
                    return Err(RegError::EEscape.into());
                }
                self.retv(T::Plain, n)
            }
            'y' => self.ret(T::WBdry),
            'Y' => self.ret(T::NWBdry),
            'Z' => self.ret(T::SEnd),
            '1'..='9' => {
                let save = self.now;
                self.now -= 1;
                let n = self.lexdigits(10, 1, 255)?;
                // "ugly heuristic (first test is "exactly 1 digit?")"
                if self.now == save || ((n as i32) > 0 && (n as i32) <= self.nsubexp as i32) {
                    return self.retv(T::Backref, n);
                }
                // Not a backref after all: an octal escape.
                self.now = save;
                self.octal()
            }
            '0' => self.octal(),
            // Unrecognized ASCII alpha escapes are reserved.
            _ => Err(RegError::EEscape.into()),
        }
    }

    /// The octal tail of `lexescape` (the first digit was just consumed).
    fn octal(&mut self) -> R {
        self.now -= 1;
        let mut n = self.lexdigits(8, 1, 3)?;
        if n > 0xff {
            self.now -= 1;
            n >>= 3;
        }
        self.retv(T::Plain, n)
    }

    /// `lexdigits` — unsigned arithmetic wraps like PG's `uchr`.
    fn lexdigits(&mut self, base: u32, minlen: usize, maxlen: usize) -> R<u32> {
        let mut n: u32 = 0;
        let mut len = 0;
        while len < maxlen && !self.ateos() {
            let c = self.s[self.now];
            let d = char::from_u32(c).and_then(|c| c.to_digit(16));
            match d {
                Some(d) if d < base => {
                    self.now += 1;
                    n = n.wrapping_mul(base).wrapping_add(d);
                }
                _ => break,
            }
            len += 1;
        }
        if len < minlen {
            return Err(RegError::EEscape.into());
        }
        Ok(n)
    }

    /// `brenext` — BRE tokens.
    fn brenext(&mut self, c: u32) -> R {
        match char::from_u32(c).unwrap_or('\0') {
            '*' => {
                if matches!(self.lasttype, T::Empty | T::LParen | T::Caret) {
                    return self.retv(T::Plain, c);
                }
                return self.retv(T::Star, 1);
            }
            '[' => return self.open_bracket(),
            '.' => return self.ret(T::Dot),
            '^' => {
                if matches!(self.lasttype, T::Empty | T::LParen) {
                    return self.ret(T::Caret);
                }
                return self.retv(T::Plain, c);
            }
            '$' => {
                if self.cflags & REG_EXPANDED != 0 {
                    self.skip()?;
                }
                if self.ateos() || self.next2('\\', ')') {
                    return self.ret(T::Dollar);
                }
                return self.retv(T::Plain, c);
            }
            '\\' => {}
            _ => return self.retv(T::Plain, c),
        }
        if self.ateos() {
            return Err(RegError::EEscape.into());
        }
        let c = self.s[self.now];
        self.now += 1;
        match char::from_u32(c).unwrap_or('\0') {
            '{' => {
                self.lexcon = Con::BBnd;
                self.ret(T::LBrace)
            }
            '(' => self.retv(T::LParen, 1),
            ')' => self.retv(T::RParen, c),
            '<' => self.ret(T::Lt),
            '>' => self.ret(T::Gt),
            d @ '1'..='9' => self.retv(T::Backref, d as u32 - ch('0')),
            _ => self.retv(T::Plain, c),
        }
    }

    /// `skip` — white space and `#` comments in expanded mode.
    fn skip(&mut self) -> R {
        loop {
            while !self.ateos() && space_class(self.s[self.now]).ok_or(Stop::Unknown)? {
                self.now += 1;
            }
            if self.ateos() || self.s[self.now] != ch('#') {
                return Ok(());
            }
            while !self.ateos() && self.s[self.now] != ch('\n') {
                self.now += 1;
            }
        }
    }

    // ─── regcomp.c ──────────────────────────────────────────────────────

    /// `parse` — branches joined by `|`, up to `stopper` (`Eos` or
    /// `RParen`). `lacon` is `type == LACON`.
    fn parse(&mut self, stopper: T, lacon: bool) -> R {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Stop::Unknown);
        }
        loop {
            // parsebranch (its tail recursion through parseqatom is a loop)
            while !self.see(T::Pipe) && !self.see(stopper) && !self.see(T::Eos) {
                self.parseqatom(lacon)?;
            }
            if self.see(T::Pipe) {
                self.next()?;
            } else {
                break;
            }
        }
        if !self.see(stopper) {
            return Err(RegError::EParen.into());
        }
        self.depth -= 1;
        Ok(())
    }

    /// `parseqatom` — one quantified atom, or a constraint.
    fn parseqatom(&mut self, lacon: bool) -> R {
        match self.nexttype {
            // Constraints take no quantifier.
            T::Caret | T::Dollar | T::SBegin | T::SEnd | T::Lt | T::Gt | T::WBdry | T::NWBdry => {
                return self.next();
            }
            T::Lacon => {
                self.next()?;
                self.parse(T::RParen, true)?;
                return self.next();
            }
            T::Star | T::Plus | T::Quest | T::LBrace => return Err(RegError::BadRpt.into()),
            T::RParen => {
                // Unbalanced `)`: an ordinary character only in EREs.
                if self.cflags & REG_ADVANCED != REG_EXTENDED {
                    return Err(RegError::EParen.into());
                }
                self.next()?;
            }
            T::Plain | T::CClassS | T::CClassC | T::Dot => self.next()?,
            T::LBrack => {
                self.bracket()?;
                self.next()?;
            }
            T::LParen => {
                let cap = !lacon && self.nextvalue != 0;
                let mut subno = 0;
                if cap {
                    self.nsubexp += 1;
                    subno = self.nsubexp;
                    if subno >= self.nsubs {
                        // moresubs
                        self.nsubs = subno * 3 / 2 + 1;
                    }
                }
                self.next()?;
                self.parse(T::RParen, lacon)?;
                self.next()?;
                if cap {
                    let i = subno as usize;
                    if self.closed.len() <= i {
                        self.closed.resize(i + 1, false);
                    }
                    self.closed[i] = true;
                }
            }
            T::Backref => {
                // Lookaround constraints can't contain backrefs.
                if lacon {
                    return Err(RegError::ESubReg.into());
                }
                let subno = self.nextvalue;
                if subno >= self.nsubs || !self.closed.get(subno as usize).copied().unwrap_or(false)
                {
                    return Err(RegError::ESubReg.into());
                }
                self.next()?;
            }
            // REG_ASSERT: can't happen.
            _ => return Err(Stop::Unknown),
        }

        // ...and an atom may be followed by a quantifier.
        match self.nexttype {
            T::Star | T::Plus | T::Quest => self.next()?,
            T::LBrace => {
                self.next()?;
                let m = self.scannum()?;
                if self.see(T::Comma) {
                    self.next()?;
                    let n = if self.see(T::Digit) {
                        self.scannum()?
                    } else {
                        DUPINF
                    };
                    if m > n {
                        return Err(RegError::BadBr.into());
                    }
                }
                if !self.see(T::RBrace) {
                    return Err(RegError::BadBr.into());
                }
                self.next()?;
            }
            _ => {}
        }
        Ok(())
    }

    /// `scannum` — a bound's number, at most `DUPMAX`.
    fn scannum(&mut self) -> R<u32> {
        let mut n = 0;
        while self.see(T::Digit) && n < DUPMAX {
            n = n * 10 + self.nextvalue;
            self.next()?;
        }
        if self.see(T::Digit) || n > DUPMAX {
            return Err(RegError::BadBr.into());
        }
        Ok(n)
    }

    /// `bracket` / `cbracket` — current token is `[`.
    fn bracket(&mut self) -> R {
        self.next()?;
        while !self.see(T::RBrack) && !self.see(T::Eos) {
            self.brackpart()?;
        }
        Ok(())
    }

    /// `brackpart` — one item or range of a bracket expression.
    fn brackpart(&mut self) -> R {
        let startc = match self.nexttype {
            T::Range => return Err(RegError::ERange.into()),
            T::Plain => {
                let c = self.nextvalue;
                self.next()?;
                if !self.see(T::Range) {
                    return Ok(());
                }
                c
            }
            T::CollEl => {
                let (sp, ep) = self.scanplain()?;
                if sp >= ep {
                    return Err(RegError::ECollate.into());
                }
                self.element(sp, ep)?
            }
            T::EClass => {
                let (sp, ep) = self.scanplain()?;
                if sp >= ep {
                    return Err(RegError::ECollate.into());
                }
                self.element(sp, ep)?;
                return Ok(());
            }
            T::CClass => {
                let (sp, ep) = self.scanplain()?;
                if sp >= ep || !CLASS_NAMES.iter().any(|n| name_eq(n, &self.s[sp..ep])) {
                    return Err(RegError::ECType.into());
                }
                return Ok(());
            }
            T::CClassS | T::CClassC => return self.next(),
            // REG_ASSERT: can't happen.
            _ => return Err(Stop::Unknown),
        };

        let endc = if self.see(T::Range) {
            self.next()?;
            match self.nexttype {
                T::Plain | T::Range => {
                    let c = self.nextvalue;
                    self.next()?;
                    c
                }
                T::CollEl => {
                    let (sp, ep) = self.scanplain()?;
                    if sp >= ep {
                        return Err(RegError::ECollate.into());
                    }
                    self.element(sp, ep)?
                }
                _ => return Err(RegError::ERange.into()),
            }
        } else {
            startc
        };
        // regc_locale.c `range`: `a != b && !before(a, b)`.
        if startc > endc {
            return Err(RegError::ERange.into());
        }
        Ok(())
    }

    /// `scanplain` — the PLAIN contents of `[. .]` etc. Returns
    /// `(startp, endp)` as PG computes them.
    fn scanplain(&mut self) -> R<(usize, usize)> {
        let startp = self.now;
        self.next()?;
        let mut endp = self.now;
        while self.see(T::Plain) {
            endp = self.now;
            self.next()?;
        }
        self.next()?;
        Ok((startp, endp))
    }

    /// regc_locale.c `element` — a collating element's character.
    fn element(&self, sp: usize, ep: usize) -> R<u32> {
        let name = &self.s[sp..ep];
        if name.len() == 1 {
            return Ok(name[0]);
        }
        CNAMES
            .iter()
            .find(|(n, _)| name_eq(n, name))
            .map(|&(_, c)| c as u32)
            .ok_or(RegError::ECollate.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARE: u32 = REG_ADVANCED;

    fn err(p: &str) -> Option<&'static str> {
        compile(p, ARE).map(RegError::text)
    }

    #[test]
    fn accepts_valid_patterns() {
        for p in [
            "",
            "a",
            "a|b|",
            "()",
            "(a)(b)\\2\\1",
            "(?:a)+?",
            "a{0,255}",
            "a{,3}",
            "a{3}?",
            "[]a]",
            "[^]a]",
            "[a-]",
            "[-a]",
            "[[:alpha:][:digit:]]",
            "[[.hyphen.]-[.period.]]",
            "[[=a=]]",
            "[\\d\\W]",
            "[[:<:]]a[[:>:]]",
            "\\ma\\M\\y\\Y\\A\\Z",
            "(?=a)(?!b)(?<=c)(?<!d)",
            "(?i)ABC",
            "(?x) a b # comment\n c",
            "(?e))",
            "(?b)\\(a\\)\\1*",
            "(?q)(((",
            "***=(((",
            "***:a",
            "a(?#comment)b",
            "\\x41\\u0041\\U00000041\\0\\101\\cA",
            "(a)\\10",
            "\\12",
            "\\U7FFFFFFE",
            "x{",
            "a{1,2}{",
        ] {
            assert_eq!(err(p), None, "{p}");
        }
    }

    #[test]
    fn rejects_like_pg() {
        for (p, e) in [
            ("(", RegError::EParen),
            (")", RegError::EParen),
            ("a)", RegError::EParen),
            ("[a", RegError::EBrack),
            ("[]", RegError::EBrack),
            ("a{1", RegError::EBrace),
            ("a{1a}", RegError::BadBr),
            ("a{256}", RegError::BadBr),
            ("a{2,1}", RegError::BadBr),
            ("*", RegError::BadRpt),
            ("a**", RegError::BadRpt),
            ("^*", RegError::BadRpt),
            ("a(?i)b", RegError::BadRpt),
            ("(?<x)", RegError::BadRpt),
            ("***", RegError::BadRpt),
            ("***?", RegError::BadPat),
            ("(?z)", RegError::BadOpt),
            ("(?i", RegError::BadOpt),
            ("\\", RegError::EEscape),
            ("\\g", RegError::EEscape),
            ("\\x", RegError::EEscape),
            ("\\u12", RegError::EEscape),
            ("\\UFFFFFFFF", RegError::EEscape),
            ("\\99", RegError::EEscape),
            ("[\\y]", RegError::EEscape),
            ("\\1", RegError::ESubReg),
            ("(a\\1)", RegError::ESubReg),
            ("(a)(?=\\1)", RegError::ESubReg),
            ("[z-a]", RegError::ERange),
            ("[a-b-c]", RegError::ERange),
            ("[a-\\d]", RegError::ERange),
            ("[[:foo:]]", RegError::ECType),
            ("[[.foo.]]", RegError::ECollate),
            ("[[.a.]", RegError::EBrack),
        ] {
            assert_eq!(compile(p, ARE), Some(e), "{p}");
        }
    }

    #[test]
    fn quote_and_ere_flags() {
        assert_eq!(compile("(", REG_QUOTE), None);
        assert_eq!(compile(")", REG_EXTENDED), None);
        assert_eq!(compile("\\g", REG_EXTENDED), None);
        assert_eq!(
            check("(", ARE),
            Err("invalid regular expression: parentheses () not balanced".to_string())
        );
    }
}
