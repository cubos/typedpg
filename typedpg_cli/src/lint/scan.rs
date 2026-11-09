//! Finding the `sql!` and `copy_in!` invocations of a Rust source.
//!
//! The scanner sees syntax only: a macro whose path ends in `sql` is a
//! query and one ending in `copy_in` a COPY target, and the same grammars
//! as the macros' inputs read their arguments up to the string literal.
//! Other macros are descended into on a best-effort basis (`assert!`,
//! `vec!`, `tokio::try_join!` and the like often wrap a query), by parsing
//! their tokens as expressions and as statements. A `sql!` whose SQL
//! argument is not a literal is an error, as the macro's own parse is.

use std::collections::{HashMap, HashSet};

use proc_macro2::{Span, TokenStream};
use syn::PathArguments;
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Block, Expr, Ident, LitStr, Macro, Pat, Stmt, Token};

/// A query found at one `sql!` site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoundQuery {
    /// The named database (`db = name`), or `None` for the default one.
    pub db: Option<String>,
    /// The literal's cooked value: the SQL the macro analyzes.
    pub text: String,
    /// Where the literal is in the file.
    pub literal: Literal,
    /// The bindings after the literal, exactly as the macro parses them;
    /// empty when that parse fails (the SQL is still linted).
    pub bindings: Vec<Binding>,
    /// The methods called on the query, in source order: directly on the
    /// macro, or on a plain local binding of it.
    pub calls: Vec<Call>,
    /// How many uses of the query the scan saw, method calls included.
    pub uses: usize,
    /// Whether a use is not a method call the scan recognized (`f(q)`,
    /// `&q`, a reassignment, …). `unused_returning` stays quiet then: the
    /// query may be used in a way the calls do not show.
    pub escapes: bool,
}

/// A method called on a query expression or a local binding of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    /// The method's name (`fetch_one`, `fetch_optional`, `fetch_all`,
    /// `execute`, …).
    pub method: String,
    /// File byte range of the method identifier in the chain.
    pub span: std::ops::Range<usize>,
}

/// One `name = expr` or bare `name` binding of a `sql!` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    /// The parameter's name as the macro reads it (`Ident::to_string()`).
    pub name: String,
    /// What the binding maps the parameter to.
    pub value: BindingValue,
    /// File byte range of the binding (its name through the end of its
    /// expression).
    pub span: std::ops::Range<usize>,
    /// File byte range that deletes the binding: from the comma before it
    /// through the binding's end.
    pub removal: std::ops::Range<usize>,
}

/// The value side of a [`Binding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindingValue {
    /// A bare `name`: capture the variable of the same name.
    Capture,
    /// An expression that is exactly one plain variable, stored by name.
    Variable(String),
    /// Anything else (`&x`, `x.y`, `x.clone()`, a literal, …).
    Other,
}

/// A target found at one `copy_in!` site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoundCopyIn {
    /// The named database (`db = name`), or `None` for the default one.
    pub db: Option<String>,
    /// The literal's cooked value: the `table (columns)` the macro analyzes.
    pub target: String,
    /// Where the literal is in the file.
    pub literal: Literal,
}

/// A `sql!` the macro itself rejects before analyzing: its SQL argument is
/// not a string literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoundError {
    /// The byte offset in the file of the offending argument.
    pub offset: usize,
    pub message: String,
}

/// Everything a Rust source holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scan {
    pub queries: Vec<FoundQuery>,
    pub copies: Vec<FoundCopyIn>,
    pub errors: Vec<FoundError>,
}

/// A string literal as written in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Literal {
    /// The byte offset in the file of the literal's contents (after the
    /// opening delimiter).
    pub content_start: usize,
    /// The contents as written, escapes undecoded.
    pub raw: String,
    /// The cooked value (what the macro analyzes).
    pub value: String,
    /// Whether the literal is a raw string (`r"…"`, `r#"…"#`), whose
    /// contents have no escapes to decode.
    pub raw_string: bool,
}

impl Literal {
    /// The byte offset in the file of the character at byte `cooked` of the
    /// literal's value. Past the end maps to the end.
    pub fn source_offset(&self, cooked: usize) -> usize {
        let cooked = cooked.min(self.value.len());
        if self.raw_string {
            // A raw string decodes nothing but the CRLFs Rust reads as LF.
            self.content_start + crlf_to_raw(&self.raw, cooked, self.value.len())
        } else {
            self.content_start + cooked_to_raw(&self.raw, cooked)
        }
    }
}

/// Every `sql!` and `copy_in!` in `text`, in source order; the file's parse
/// error when it does not parse.
pub(crate) fn scan(text: &str) -> Result<Scan, syn::Error> {
    let file = syn::parse_file(text)?;
    let mut finder = Finder {
        text,
        scan: Scan::default(),
        seen: HashSet::new(),
        queries: HashMap::new(),
    };
    finder.visit_file(&file);
    Ok(finder.scan)
}

struct Finder<'a> {
    text: &'a str,
    scan: Scan,
    /// The path start of each macro already recorded: parsing a macro's
    /// tokens as both expressions and statements visits it twice.
    seen: HashSet<usize>,
    /// The index in `scan.queries` of the query recorded at each macro path
    /// start, so a method call on it can attach the call site.
    queries: HashMap<usize, usize>,
}

impl Finder<'_> {
    fn record_sql(&mut self, mac: &Macro) {
        let start = span_offset(self.text, mac.path.span());
        if !self.seen.insert(start) {
            return;
        }
        let Some((db, sql, rest)) = parse_sql_arguments(mac.tokens.clone()) else {
            return;
        };
        match sql {
            Ok(sql) => {
                let Some(literal) = literal(self.text, sql.span(), &sql) else {
                    return;
                };
                let bindings = parse_bindings(self.text, rest).unwrap_or_default();
                self.queries.insert(start, self.scan.queries.len());
                self.scan.queries.push(FoundQuery {
                    db,
                    text: literal.value.clone(),
                    literal,
                    bindings,
                    calls: Vec::new(),
                    uses: 0,
                    escapes: false,
                });
            }
            Err(error) => self.scan.errors.push(FoundError {
                offset: span_offset(self.text, error.span()),
                message: "the SQL of sql! must be a string literal".to_owned(),
            }),
        }
    }

    fn record_copy_in(&mut self, mac: &Macro) {
        let start = span_offset(self.text, mac.path.span());
        if !self.seen.insert(start) {
            return;
        }
        let Some((db, target)) = parse_copy_in_arguments(mac.tokens.clone()) else {
            return;
        };
        let Some(literal) = literal(self.text, target.span(), &target) else {
            return;
        };
        self.scan.copies.push(FoundCopyIn {
            db,
            target: literal.value.clone(),
            literal,
        });
    }

    /// Visit the queries inside another macro's tokens, best effort.
    fn descend(&mut self, tokens: &TokenStream) {
        if let Ok(expressions) =
            Punctuated::<Expr, Token![,]>::parse_terminated.parse2(tokens.clone())
        {
            for expression in &expressions {
                self.visit_expr(expression);
            }
        }
        if let Ok(statements) = Block::parse_within.parse2(tokens.clone()) {
            for statement in &statements {
                self.visit_stmt(statement);
            }
        }
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_macro(&mut self, mac: &'ast Macro) {
        match mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .as_deref()
        {
            Some("sql") => self.record_sql(mac),
            Some("copy_in") => self.record_copy_in(mac),
            _ => self.descend(&mac.tokens),
        }
    }

    fn visit_block(&mut self, block: &'ast Block) {
        self.record_bindings(block);
        syn::visit::visit_block(self, block);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // A method called on the macro expression names a call site of the
        // query the macro records. The receiver is visited first, so the
        // query exists by the time the call is linked.
        if let Expr::MethodCall(method) = expr
            && let Some(mac) = receiver_macro(&method.receiver)
            && is_sql_macro(mac)
        {
            let start = span_offset(self.text, mac.path.span());
            let method_span = method.method.span();
            let name = method.method.to_string();
            let span = span_offset(self.text, method_span)..span_end_offset(self.text, method_span);
            syn::visit::visit_expr(self, expr);
            if let Some(&index) = self.queries.get(&start) {
                let query = &mut self.scan.queries[index];
                query.calls.push(Call { method: name, span });
                query.uses += 1;
            }
            return;
        }
        syn::visit::visit_expr(self, expr);
    }
}

impl Finder<'_> {
    /// Record the calls on a `let q = sql!(…);` in `block`: its following
    /// statements, up to a shadowing binding or a reassignment of `q`.
    fn record_bindings(&mut self, block: &Block) {
        for (position, statement) in block.stmts.iter().enumerate() {
            let Stmt::Local(local) = statement else {
                continue;
            };
            let Some(name) = plain_ident_pattern(&local.pat) else {
                continue;
            };
            let Some(init) = &local.init else {
                continue;
            };
            let Some(mac) = receiver_macro(&init.expr) else {
                continue;
            };
            if !is_sql_macro(mac) {
                continue;
            }
            self.record_sql(mac);
            let start = span_offset(self.text, mac.path.span());
            let Some(&index) = self.queries.get(&start) else {
                continue;
            };
            let uses = binding_uses(self.text, &name, &block.stmts[position + 1..]);
            let query = &mut self.scan.queries[index];
            query.calls.extend(uses.calls);
            query.uses += uses.uses;
            query.escapes |= uses.escapes;
        }
    }
}

/// Whether `mac` is a `sql!` invocation (its path ends in `sql`).
fn is_sql_macro(mac: &Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "sql")
}

/// The name a plain `let` pattern binds (`let q = …`), if it is one.
fn plain_ident_pattern(pattern: &Pat) -> Option<String> {
    let Pat::Ident(ident) = pattern else {
        return None;
    };
    if ident.subpat.is_some() {
        return None;
    }
    Some(ident.ident.to_string())
}

/// Whether `pattern` binds `name` anywhere (`let (q, x) = …` shadows it too).
fn pattern_binds(pattern: &Pat, name: &str) -> bool {
    match pattern {
        Pat::Ident(ident) => ident.ident == name,
        Pat::Tuple(tuple) => tuple.elems.iter().any(|pat| pattern_binds(pat, name)),
        Pat::TupleStruct(tuple) => tuple.elems.iter().any(|pat| pattern_binds(pat, name)),
        Pat::Struct(structure) => structure
            .fields
            .iter()
            .any(|field| pattern_binds(&field.pat, name)),
        Pat::Slice(slice) => slice.elems.iter().any(|pat| pattern_binds(pat, name)),
        Pat::Or(or) => or.cases.iter().any(|pat| pattern_binds(pat, name)),
        Pat::Paren(paren) => pattern_binds(&paren.pat, name),
        Pat::Reference(reference) => pattern_binds(&reference.pat, name),
        Pat::Type(typed) => pattern_binds(&typed.pat, name),
        _ => false,
    }
}

/// The variable `expr` is, through parentheses, when it is a plain path.
fn receiver_name(expr: &Expr) -> Option<&Ident> {
    match expr {
        Expr::Path(path)
            if path.attrs.is_empty()
                && path.qself.is_none()
                && path.path.leading_colon.is_none()
                && path.path.segments.len() == 1
                && matches!(path.path.segments[0].arguments, PathArguments::None) =>
        {
            Some(&path.path.segments[0].ident)
        }
        Expr::Paren(paren) => receiver_name(&paren.expr),
        Expr::Group(group) => receiver_name(&group.expr),
        _ => None,
    }
}

/// What the scan saw of a bound query's uses.
#[derive(Default)]
struct Uses {
    calls: Vec<Call>,
    uses: usize,
    escapes: bool,
}

/// Walk `statements` (the rest of the block after `let name = sql!(…)`) and
/// collect the calls on `name`, stopping at a shadowing binding or a
/// reassignment.
fn binding_uses(text: &str, name: &str, statements: &[Stmt]) -> Uses {
    let mut finder = BindingUses {
        text,
        name,
        uses: Uses::default(),
        stop: false,
    };
    for statement in statements {
        if finder.stop {
            break;
        }
        finder.visit_stmt(statement);
    }
    finder.uses
}

struct BindingUses<'a> {
    text: &'a str,
    name: &'a str,
    uses: Uses,
    /// A shadowing binding or a reassignment was seen: the rest of this
    /// block is about another value.
    stop: bool,
}

impl<'ast> Visit<'ast> for BindingUses<'_> {
    fn visit_block(&mut self, block: &'ast Block) {
        // A shadowing binding inside the block only hides the outer one
        // until the block ends.
        let stop = self.stop;
        self.stop = false;
        for statement in &block.stmts {
            if self.stop {
                break;
            }
            self.visit_stmt(statement);
        }
        self.stop = stop;
    }

    fn visit_stmt(&mut self, statement: &'ast Stmt) {
        if self.stop {
            return;
        }
        if let Stmt::Local(local) = statement {
            // The initializer runs before the new binding takes effect.
            if let Some(init) = &local.init {
                self.visit_expr(&init.expr);
            }
            if pattern_binds(&local.pat, self.name) {
                self.stop = true;
            }
            return;
        }
        syn::visit::visit_stmt(self, statement);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if self.stop {
            return;
        }
        match expr {
            Expr::MethodCall(method) => {
                if receiver_name(&method.receiver).is_some_and(|name| name == self.name) {
                    self.uses.uses += 1;
                    let span = method.method.span();
                    self.uses.calls.push(Call {
                        method: method.method.to_string(),
                        span: span_offset(self.text, span)..span_end_offset(self.text, span),
                    });
                    for argument in &method.args {
                        self.visit_expr(argument);
                    }
                    return;
                }
                syn::visit::visit_expr(self, expr);
            }
            Expr::Assign(assign) => {
                if receiver_name(&assign.left).is_some_and(|name| name == self.name) {
                    self.uses.uses += 1;
                    self.uses.escapes = true;
                    self.stop = true;
                    return;
                }
                syn::visit::visit_expr(self, expr);
            }
            Expr::Path(_) => {
                if receiver_name(expr).is_some_and(|name| name == self.name) {
                    self.uses.uses += 1;
                    self.uses.escapes = true;
                    return;
                }
                syn::visit::visit_expr(self, expr);
            }
            _ => syn::visit::visit_expr(self, expr),
        }
    }
}

/// The `sql!` macro expression `expr` is, through parentheses and the
/// invisible groups macro input can create.
fn receiver_macro(expr: &Expr) -> Option<&Macro> {
    match expr {
        Expr::Macro(mac) => Some(&mac.mac),
        Expr::Paren(paren) => receiver_macro(&paren.expr),
        Expr::Group(group) => receiver_macro(&group.expr),
        _ => None,
    }
}

/// `[db = name,]` from the macro arguments.
fn parse_db(input: ParseStream<'_>) -> syn::Result<Option<String>> {
    if input.peek(Ident) {
        let fork = input.fork();
        let ident: Ident = fork.parse()?;
        if ident == "db" && fork.peek(Token![=]) {
            let _: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let name: Ident = input.parse()?;
            input.parse::<Token![,]>()?;
            return Ok(Some(name.to_string()));
        }
    }
    Ok(None)
}

/// `(db, sql, rest)` from `[db = name,] executor, "SQL", …`: the literal,
/// and the tokens after it. The literal is an error when the argument is not
/// a string literal: the macro rejects it.
fn parse_sql_arguments(
    tokens: TokenStream,
) -> Option<(Option<String>, syn::Result<LitStr>, TokenStream)> {
    let parser =
        |input: ParseStream<'_>| -> syn::Result<(Option<String>, syn::Result<LitStr>, TokenStream)> {
            let db = parse_db(input)?;
            let _: Expr = input.parse()?;
            input.parse::<Token![,]>()?;
            let sql = input.parse::<LitStr>();
            // Whatever follows the literal: the `sql!` bindings.
            let rest: TokenStream = input.parse()?;
            Ok((db, sql, rest))
        };
    parser.parse2(tokens).ok()
}

/// The bindings after the literal, as the macro's `QueryInput::parse` reads
/// them: each is preceded by a comma and is a bare `name` or `name = expr`.
fn parse_bindings(text: &str, tokens: TokenStream) -> Option<Vec<Binding>> {
    let parser = |input: ParseStream<'_>| -> syn::Result<Vec<Binding>> {
        let mut bindings = Vec::new();
        while input.peek(Token![,]) {
            let comma: Token![,] = input.parse()?;
            let comma_start = span_offset(text, comma.span());
            if input.is_empty() {
                break;
            }
            let name: Ident = input.parse()?;
            let name_start = span_offset(text, name.span());
            let (value, end) = if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                let expr: Expr = input.parse()?;
                (binding_value(&expr), span_end_offset(text, expr.span()))
            } else {
                (BindingValue::Capture, span_end_offset(text, name.span()))
            };
            bindings.push(Binding {
                name: name.to_string(),
                value,
                span: name_start..end,
                removal: comma_start..end,
            });
        }
        if input.is_empty() {
            Ok(bindings)
        } else {
            Err(input.error("unexpected tokens after the last binding"))
        }
    };
    parser.parse2(tokens).ok()
}

/// The variable a binding's expression is, when it is exactly one plain
/// variable: no attributes, `qself`, leading `::`, qualification or
/// arguments, and an ordinary identifier that is not a keyword-path.
fn binding_value(expr: &Expr) -> BindingValue {
    let Expr::Path(path) = expr else {
        return BindingValue::Other;
    };
    if !path.attrs.is_empty() || path.qself.is_some() || path.path.leading_colon.is_some() {
        return BindingValue::Other;
    }
    let mut segments = path.path.segments.iter();
    let Some(segment) = segments.next() else {
        return BindingValue::Other;
    };
    if segments.next().is_some() || !matches!(segment.arguments, PathArguments::None) {
        return BindingValue::Other;
    }
    let name = segment.ident.to_string();
    if name.starts_with("r#") || matches!(name.as_str(), "self" | "Self" | "super" | "crate") {
        return BindingValue::Other;
    }
    BindingValue::Variable(name)
}

/// `(db, target)` from `[db = name,] executor, "table (columns)", …`, the
/// same grammar as the macro's `CopyInInput`.
fn parse_copy_in_arguments(tokens: TokenStream) -> Option<(Option<String>, LitStr)> {
    let parser = |input: ParseStream<'_>| -> syn::Result<(Option<String>, LitStr)> {
        let db = parse_db(input)?;
        let _: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let target: LitStr = input.parse()?;
        // The source and its `{ fields }` are irrelevant here.
        let _: TokenStream = input.parse()?;
        Ok((db, target))
    };
    parser.parse2(tokens).ok()
}

/// The literal at byte `start` of `text`, as written.
fn literal(text: &str, span: Span, lit: &LitStr) -> Option<Literal> {
    let start = span_offset(text, span);
    let token = lit.token().to_string();
    let (content_start, raw, raw_string) = if let Some(rest) = token.strip_prefix('r') {
        let hashes = rest.bytes().take_while(|byte| *byte == b'#').count();
        let quote = 1 + hashes;
        if token.as_bytes().get(quote) != Some(&b'"') {
            return None;
        }
        let end = token.len().checked_sub(1 + hashes)?;
        (
            start + quote + 1,
            token.get(quote + 1..end)?.to_owned(),
            true,
        )
    } else {
        if !token.starts_with('"') || !token.ends_with('"') {
            return None;
        }
        (start + 1, token.get(1..token.len() - 1)?.to_owned(), false)
    };
    Some(Literal {
        content_start,
        raw,
        value: lit.value(),
        raw_string,
    })
}

/// The byte offset of a span's start (1-based line, 0-based char column) in
/// `text`.
fn span_offset(text: &str, span: Span) -> usize {
    let start = span.start();
    line_column_to_offset(text, start.line, start.column).unwrap_or(0)
}

/// The byte offset of a span's end (exclusive) in `text`.
fn span_end_offset(text: &str, span: Span) -> usize {
    let end = span.end();
    line_column_to_offset(text, end.line, end.column).unwrap_or(text.len())
}

fn line_column_to_offset(text: &str, line: usize, column: usize) -> Option<usize> {
    let mut line_start = 0usize;
    let mut current_line = 1usize;
    for (i, c) in text.char_indices() {
        if current_line == line {
            break;
        }
        if c == '\n' {
            current_line += 1;
            line_start = i + 1;
        }
    }
    if current_line != line {
        return None;
    }
    let mut offset = line_start;
    for _ in 0..column {
        offset += text.get(offset..)?.chars().next()?.len_utf8();
    }
    Some(offset)
}

/// The byte offset in `raw` (a normal string's contents as written) of the
/// character at byte `cooked` of its value. Past the end maps to the end.
///
/// Decodes the escapes a Rust string literal has: `\n`, `\r`, `\t`, `\\`,
/// `\0`, `\'`, `\"`, `\xNN`, `\u{…}`, and the line continuation (`\` and
/// the newline and whitespace after it), which produces nothing.
pub(crate) fn cooked_to_raw(raw: &str, cooked: usize) -> usize {
    let mut i = 0usize;
    let mut produced = 0usize;
    while i < raw.len() {
        let c = raw[i..].chars().next().expect("in bounds");
        if c != '\\' {
            if produced >= cooked {
                return i;
            }
            // A physical CRLF reads as one LF (rustc and syn normalize it).
            if raw[i..].starts_with("\r\n") {
                i += 2;
                produced += 1;
                continue;
            }
            i += c.len_utf8();
            produced += c.len_utf8();
            continue;
        }
        let rest = &raw[i + 1..];
        // A line continuation and its following whitespace produce nothing:
        // they are stepped over whatever the target offset is.
        match rest.chars().next() {
            Some('\n') => {
                i += 2;
                i = skip_whitespace(raw, i);
                continue;
            }
            Some('\r') if rest.starts_with("\r\n") => {
                i += 3;
                i = skip_whitespace(raw, i);
                continue;
            }
            _ => {}
        }
        if produced >= cooked {
            return i;
        }
        match rest.chars().next() {
            Some('n' | 'r' | 't' | '\\' | '0' | '\'' | '"') => {
                i += 2;
                produced += 1;
            }
            Some('x') => {
                let hex = raw.get(i + 2..i + 4).unwrap_or("00");
                let codepoint = u32::from_str_radix(hex, 16).unwrap_or(0);
                i = (i + 4).min(raw.len());
                produced += char::from_u32(codepoint).map_or(1, char::len_utf8);
            }
            Some('u') if raw.get(i + 2..i + 3) == Some("{") => {
                let Some(end) = raw[i + 3..].find('}').map(|end| i + 3 + end) else {
                    i = (i + 2).min(raw.len());
                    produced += 1;
                    continue;
                };
                // Rust allows underscores between the digits: `\u{0_061}`.
                let digits: String = raw[i + 3..end].chars().filter(|&c| c != '_').collect();
                let codepoint = u32::from_str_radix(&digits, 16).unwrap_or(0xFFFD);
                let c = char::from_u32(codepoint).unwrap_or('\u{FFFD}');
                i = end + 1;
                produced += c.len_utf8();
            }
            Some(other) => {
                i += 1 + other.len_utf8();
                produced += other.len_utf8();
            }
            None => {
                i += 1;
                produced += 1;
            }
        }
    }
    raw.len()
}

/// The byte in a raw string's `raw` contents of byte `cooked` of its value,
/// whose length is `value_len`: a CRLF reads as one LF (rustc and syn
/// normalize it), everything else as written.
fn crlf_to_raw(raw: &str, cooked: usize, value_len: usize) -> usize {
    if raw.len() == value_len {
        return cooked;
    }
    let bytes = raw.as_bytes();
    let (mut i, mut produced) = (0, 0);
    while i < bytes.len() && produced < cooked {
        i += if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            2
        } else {
            1
        };
        produced += 1;
    }
    i
}

fn skip_whitespace(raw: &str, mut i: usize) -> usize {
    while let Some(c) = raw.get(i..).and_then(|rest| rest.chars().next()) {
        if !c.is_whitespace() {
            break;
        }
        i += c.len_utf8();
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(source: &str) -> Scan {
        scan(source).unwrap_or_else(|_| panic!("does not parse: {source}"))
    }

    fn queries(source: &str) -> Vec<FoundQuery> {
        scanned(source).queries
    }

    #[test]
    fn finds_queries_and_their_databases() {
        let source = r#"
fn main() {
    let _ = sql!(pool, "SELECT 1", id = 1);
    let _ = typedpg::sql!(pool, "SELECT 2");
    let _ = sql!(db = other, pool, "SELECT 3");
    assert!(sql!(pool, "SELECT 4") == sql!(pool, "SELECT 5"));
    let _ = vec![sql!(pool, "SELECT 6")];
}
"#;
        let found = queries(source);
        let db: Vec<Option<&str>> = found.iter().map(|query| query.db.as_deref()).collect();
        let sql: Vec<&str> = found.iter().map(|query| query.text.as_str()).collect();
        assert_eq!(db, [None, None, Some("other"), None, None, None]);
        assert_eq!(
            sql,
            [
                "SELECT 1", "SELECT 2", "SELECT 3", "SELECT 4", "SELECT 5", "SELECT 6"
            ]
        );
    }

    #[test]
    fn finds_copy_in_targets_with_their_databases() {
        let source = r##"
fn main() {
    copy_in!(pool, "users (id)", rows { id });
    copy_in!(db = other, &tx, r#"things (name)"#, rows.iter() { name },);
}
"##;
        let found = scanned(source);
        assert!(found.queries.is_empty());
        assert!(found.errors.is_empty());
        let db: Vec<Option<&str>> = found.copies.iter().map(|copy| copy.db.as_deref()).collect();
        let targets: Vec<&str> = found
            .copies
            .iter()
            .map(|copy| copy.target.as_str())
            .collect();
        assert_eq!(db, [None, Some("other")]);
        assert_eq!(targets, ["users (id)", "things (name)"]);
    }

    #[test]
    fn a_non_literal_sql_argument_is_an_error() {
        let source = r#"
fn main() {
    sql!(pool, query);
    sql!(pool, concat!("SELECT 1"));
    sql!(pool, 42);
}
"#;
        let found = scanned(source);
        assert!(found.queries.is_empty());
        assert_eq!(found.errors.len(), 3);
        for error in &found.errors {
            assert_eq!(error.message, "the SQL of sql! must be a string literal");
            assert!(error.offset < source.len());
        }
        assert!(source[found.errors[0].offset..].starts_with("query"));
        assert!(source[found.errors[1].offset..].starts_with("concat"));
    }

    #[test]
    fn cooked_offsets_map_through_every_escape() {
        let source = r#"fn main() {
    let _ = sql!(pool, "a\nb\tc\rd\\e\0f\'g\"h\x41i\u{1F600}j");
}"#;
        let query = &queries(source)[0];
        assert_eq!(query.text, "a\nb\tc\rd\\e\0f'g\"hAi😀j");
        for (needle, written) in [
            ("\n", "\\n"),
            ("\t", "\\t"),
            ("\r", "\\r"),
            ("\\", "\\\\"),
            ("\0", "\\0"),
            ("'", "\\'"),
            ("\"", "\\\""),
            ("A", "\\x41"),
            ("😀", "\\u{1F600}"),
        ] {
            let at = query.text.find(needle).unwrap();
            let file_at = query.literal.source_offset(at);
            assert!(
                source[file_at..].starts_with(written),
                "{needle:?} maps to {:?}, not {written:?}",
                &source[file_at..]
            );
        }
        // The end of the value is the closing quote.
        let end = query.literal.source_offset(query.text.len());
        assert!(source[end..].starts_with('"'));
    }

    #[test]
    fn a_physical_crlf_in_a_literal_reads_as_one_lf() {
        for source in [
            "fn main() { sql!(pool, \"SELECT\r\n1 AS one LIMIT 1\"); }",
            "fn main() { sql!(pool, r#\"SELECT\r\n1 AS one LIMIT 1\"#); }",
        ] {
            let scanned = scan(source).unwrap();
            let query = &scanned.queries[0];
            // syn normalizes the CRLF of a normal literal; a raw one keeps
            // it. Either way the offsets must land on the same text.
            assert!(query.text.ends_with("1 AS one LIMIT 1"), "{source:?}");
            let at = query.text.find(" LIMIT").unwrap();
            let offset = query.literal.source_offset(at);
            assert!(source[offset..].starts_with(" LIMIT 1"), "{source:?}");
        }
    }

    #[test]
    fn a_unicode_escape_with_underscores_is_one_character() {
        let raw = r"\u{0_061}b";
        assert_eq!(cooked_to_raw(raw, 1), raw.find('b').unwrap());
    }

    #[test]
    fn a_line_continuation_before_the_site_maps_to_the_sql() {
        let source = r#"fn main() {
    let _ = sql!(pool, "SELECT \
        id FROM users WHERE email = NULL");
}
"#;
        let query = &queries(source)[0];
        assert_eq!(query.text, "SELECT id FROM users WHERE email = NULL");
        let at = query.literal.source_offset(query.text.find("id").unwrap());
        assert!(source[at..].starts_with("id FROM"), "{}", &source[at..]);
        let at = query
            .literal
            .source_offset(query.text.find("email").unwrap());
        assert!(source[at..].starts_with("email = NULL"));
        assert_eq!(cooked_to_raw("a\\\n  b", 1), 5);
        assert_eq!(cooked_to_raw("a\\\n  b", 2), 6);
    }

    #[test]
    fn multi_byte_characters_before_the_literal_keep_the_columns_right() {
        let source = r#"fn main() { let _ = ("é", sql!(pool, "SELECT id FROM users WHERE email = NULL")); }"#;
        let query = &queries(source)[0];
        let at = query
            .literal
            .source_offset(query.text.find("email").unwrap());
        assert!(source[at..].starts_with("email = NULL"));
    }

    #[test]
    fn raw_strings_with_hashes_have_no_escapes() {
        let source = r####"fn main() {
    let _ = sql!(pool, r###"SELECT id FROM users WHERE email = NULL"###);
}"####;
        let query = &queries(source)[0];
        assert_eq!(query.text, "SELECT id FROM users WHERE email = NULL");
        assert!(query.literal.raw_string);
        let at = query
            .literal
            .source_offset(query.text.find("email").unwrap());
        assert!(source[at..].starts_with("email = NULL"));
        let end = query.literal.source_offset(query.text.len());
        assert!(source[end..].starts_with("\"###"));
    }

    #[test]
    fn an_unparseable_file_is_an_error() {
        assert!(scan("fn main( {").is_err());
    }

    #[test]
    fn bindings_read_their_values_and_spans() {
        let source = r#"fn main() {
    let _ = sql!(pool, "SELECT 1", x = x, b, a = c, d = e.f, f = &g);
}"#;
        let query = &queries(source)[0];
        let names: Vec<&str> = query
            .bindings
            .iter()
            .map(|binding| binding.name.as_str())
            .collect();
        assert_eq!(names, ["x", "b", "a", "d", "f"]);
        let values: Vec<&BindingValue> = query
            .bindings
            .iter()
            .map(|binding| &binding.value)
            .collect();
        assert_eq!(
            values,
            [
                &BindingValue::Variable("x".into()),
                &BindingValue::Capture,
                &BindingValue::Variable("c".into()),
                &BindingValue::Other,
                &BindingValue::Other,
            ]
        );
        let written: Vec<&str> = query
            .bindings
            .iter()
            .map(|binding| &source[binding.span.clone()])
            .collect();
        assert_eq!(written, ["x = x", "b", "a = c", "d = e.f", "f = &g"]);
    }

    #[test]
    fn a_binding_span_ends_after_a_multi_token_expression() {
        let source = r#"fn main() {
    let _ = sql!(pool, "SELECT 1", a = b.c(1, 2),);
}"#;
        let query = &queries(source)[0];
        assert_eq!(&source[query.bindings[0].span.clone()], "a = b.c(1, 2)");
        assert!(source[query.bindings[0].span.end..].starts_with(",);"));
    }

    /// Delete every binding from `source` through its removal range.
    fn without_bindings(source: &str, query: &FoundQuery) -> String {
        let mut text = source.to_owned();
        for binding in query.bindings.iter().rev() {
            text.replace_range(binding.removal.clone(), "");
        }
        text
    }

    #[test]
    fn removal_ranges_leave_the_remaining_arguments() {
        // The trailing comma after the last binding stays, the way the macro
        // accepts it: a lone comma after the literal.
        let source = r#"fn main() { let _ = sql!(pool, "SELECT $a", a = a, b = c,); }"#;
        let query = &queries(source)[0];
        assert_eq!(
            without_bindings(source, query),
            r#"fn main() { let _ = sql!(pool, "SELECT $a",); }"#
        );

        // No trailing comma after the last binding.
        let source = r#"fn main() { let _ = sql!(pool, "SELECT $a", a = a); }"#;
        let query = &queries(source)[0];
        assert_eq!(
            without_bindings(source, query),
            r#"fn main() { let _ = sql!(pool, "SELECT $a"); }"#
        );
    }

    #[test]
    fn removal_ranges_span_lines_and_a_database_prefix() {
        let source = r#"fn main() {
    let _ = sql!(
        db = other,
        pool,
        "SELECT $a, $b",
        a = a,
        b = next,
    );
}
"#;
        let query = &queries(source)[0];
        assert_eq!(query.db.as_deref(), Some("other"));
        assert_eq!(
            without_bindings(source, query),
            r#"fn main() {
    let _ = sql!(
        db = other,
        pool,
        "SELECT $a, $b",
    );
}
"#
        );
    }

    #[test]
    fn malformed_bindings_keep_the_query_without_bindings() {
        let source = r#"fn main() { let _ = sql!(pool, "SELECT 1", 1 = 2); }"#;
        let query = &queries(source)[0];
        assert_eq!(query.text, "SELECT 1");
        assert!(query.bindings.is_empty());
    }

    #[test]
    fn a_method_called_on_the_macro_is_recorded() {
        for (source, method) in [
            (
                r#"fn main() { let _ = sql!(pool, "SELECT 1").fetch_one().await; }"#,
                "fetch_one",
            ),
            (
                r#"fn main() { let _ = (sql!(pool, "SELECT 1")).execute(); }"#,
                "execute",
            ),
            (
                r#"fn main() {
    let _ = sql!(pool, "SELECT 1")
        .fetch_optional()
        .await;
}"#,
                "fetch_optional",
            ),
            (
                r#"fn main() {
    let _ = sql!(pool, "SELECT 1")
        .fetch_all()
        .await?;
}"#,
                "fetch_all",
            ),
            (
                r#"fn main() { let _ = sql!(pool, "SELECT 1").fetch_one_as::<T>(); }"#,
                "fetch_one_as",
            ),
        ] {
            let found = queries(source);
            assert_eq!(found.len(), 1, "{source}");
            assert_eq!(found[0].calls.len(), 1, "{source}");
            let call = &found[0].calls[0];
            assert_eq!(call.method, method, "{source}");
            assert_eq!(&source[call.span.clone()], method, "{source}");
            assert_eq!(found[0].uses, 1, "{source}");
            assert!(!found[0].escapes, "{source}");
        }
    }

    #[test]
    fn a_method_on_a_bound_query_is_a_call_on_the_query() {
        let source = r#"fn main() {
    let query = sql!(pool, "SELECT 1");
    let _ = query.fetch_one().await;
}"#;
        let query = &queries(source)[0];
        assert_eq!(query.calls.len(), 1);
        assert_eq!(query.calls[0].method, "fetch_one");
        assert_eq!(&source[query.calls[0].span.clone()], "fetch_one");
        assert_eq!(query.uses, 1);
        assert!(!query.escapes);
    }

    #[test]
    fn every_call_in_branches_is_collected() {
        let source = r#"fn main() {
    let query = sql!(pool, "SELECT 1");
    if cond {
        query.fetch_one().await?;
    } else {
        query.fetch_optional().await?;
    }
}"#;
        let query = &queries(source)[0];
        let methods: Vec<&str> = query
            .calls
            .iter()
            .map(|call| call.method.as_str())
            .collect();
        assert_eq!(methods, ["fetch_one", "fetch_optional"]);
        assert_eq!(query.uses, 2);
        assert!(!query.escapes);
    }

    #[test]
    fn a_shadowing_binding_stops_the_calls() {
        let source = r#"fn main() {
    let query = sql!(pool, "SELECT 1");
    let _ = query.fetch_one().await?;
    let query = sql!(pool, "SELECT 2");
    let _ = query.execute().await?;
}"#;
        let found = queries(source);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].calls.len(), 1);
        assert_eq!(found[0].calls[0].method, "fetch_one");
        assert_eq!(found[1].calls.len(), 1);
        assert_eq!(found[1].calls[0].method, "execute");
    }

    #[test]
    fn a_reassigned_binding_stops_the_calls() {
        let source = r#"fn main() {
    let mut query = sql!(pool, "SELECT 1");
    let _ = query.fetch_one().await?;
    query = other;
    let _ = query.execute().await?;
}"#;
        let query = &queries(source)[0];
        assert_eq!(query.calls.len(), 1);
        assert_eq!(query.calls[0].method, "fetch_one");
        assert!(query.escapes);
    }

    #[test]
    fn an_escaping_binding_keeps_the_calls_and_marks_the_escape() {
        let source = r#"fn main() {
    let query = sql!(pool, "SELECT 1");
    let _ = query.fetch_one().await?;
    consume(query);
}"#;
        let query = &queries(source)[0];
        assert_eq!(query.calls.len(), 1);
        assert_eq!(query.uses, 2);
        assert!(query.escapes);
    }

    #[test]
    fn a_plain_query_has_no_call_site() {
        let source = r#"fn main() { sql!(pool, "SELECT 1").await; }"#;
        let query = &queries(source)[0];
        assert!(query.calls.is_empty());
    }
}
