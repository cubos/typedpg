//! Finding the queries of a TypeScript source.
//!
//! A query is a call to the `sql` (or `copyIn`) a generated module exports,
//! reached through an import that resolves to it: `import { sql } from
//! './db'` under any local name, `import * as db from './db'` called as
//! `db.sql(...)`, or either through modules that re-export it. Bindings are
//! resolved semantically, so a local `sql` that shadows the import is not
//! taken for it. A use the scan misses still fails to type-check — a query
//! absent from the generated module is a type error — so the scan can stay
//! conservative.

use std::collections::HashMap;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, ExportSpecifier, Expression, ImportDeclarationSpecifier,
    ModuleExportName, Statement, TaggedTemplateExpression,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::{Scoping, SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, SourceType};

/// The names a generated module exports its functions as.
pub const SQL_EXPORT: &str = "sql";
pub const COPY_IN_EXPORT: &str = "copyIn";

/// Which generated function a call is to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// `sql("SELECT ...")`
    Query,
    /// `copyIn("table (columns)")`
    CopyIn,
}

impl Kind {
    /// The function a generated module exports under `name`.
    pub fn of_export(name: &str) -> Option<Kind> {
        match name {
            SQL_EXPORT => Some(Kind::Query),
            COPY_IN_EXPORT => Some(Kind::CopyIn),
            _ => None,
        }
    }
}

/// What an imported name is, as far as typedpg is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Export {
    /// A generated module's `sql` or `copyIn`.
    Item { db: usize, kind: Kind },
    /// A generated module itself, as a namespace (`import * as db`).
    Namespace { db: usize },
}

/// A query found in a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundQuery {
    /// The index of the database whose generated module the call is to.
    pub db: usize,
    pub kind: Kind,
    /// The query's text: the literal's value.
    pub text: String,
    /// Where the literal starts.
    pub pos: Position,
    /// The literal as written, to locate an offset of `text` in the file.
    pub literal: Literal,
}

/// A string or template literal as written in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Literal {
    /// The byte offset in the file of the literal's contents (after the
    /// opening quote).
    pub content_start: usize,
    /// The contents as written, escapes undecoded.
    pub raw: String,
    pub template: bool,
}

impl Literal {
    /// The byte offset in the file of the character at byte `offset` of
    /// the literal's value.
    pub fn source_offset(&self, offset: usize) -> usize {
        self.content_start + cooked_to_raw(&self.raw, self.template, offset)
    }
}

/// A 1-based line and column (in characters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanDiagnostic {
    pub pos: Position,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileScan {
    pub queries: Vec<FoundQuery>,
    pub diagnostics: Vec<ScanDiagnostic>,
    /// The source's lines, to place a query's errors.
    pub lines: LineIndex,
}

/// Scan `source` (the file at `path`). `resolve(specifier, name)` says what
/// the module `specifier` exports as `name` (`*` for the module itself).
/// `None` when the source doesn't parse: the editor and tsc report the
/// syntax error, and the caller keeps what the file had while it is being
/// edited.
pub fn scan(
    path: &Path,
    source: &str,
    resolve: &mut dyn FnMut(&str, &str) -> Option<Export>,
) -> Option<FileScan> {
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.diagnostics.has_errors() {
        return None;
    }
    let program = parsed.program;
    let lines = LineIndex::new(source);

    // Every value import: which module and name each local binding is.
    let mut imported = Vec::new();
    for stmt in &program.body {
        let Statement::ImportDeclaration(import) = stmt else {
            continue;
        };
        if import.import_kind.is_type() {
            continue;
        }
        for s in import.specifiers.iter().flatten() {
            let (local, name) = match s {
                ImportDeclarationSpecifier::ImportSpecifier(s) if !s.import_kind.is_type() => {
                    (&s.local, export_name(&s.imported).to_owned())
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                    (&s.local, "*".to_owned())
                }
                _ => continue,
            };
            imported.push((local, import.source.value.as_str().to_owned(), name));
        }
    }
    if imported.is_empty() {
        return Some(FileScan {
            lines,
            ..FileScan::default()
        });
    }

    // Symbols are assigned by the semantic pass.
    let semantic = SemanticBuilder::new().build(&program).semantic;
    let bindings: HashMap<SymbolId, Binding> = imported
        .into_iter()
        .filter_map(|(local, spec, name)| Some((local.symbol_id.get()?, Binding { spec, name })))
        .collect();

    let mut finder = Finder {
        scoping: semantic.scoping(),
        bindings: &bindings,
        resolve,
        cache: HashMap::new(),
        lines: &lines,
        queries: Vec::new(),
        diagnostics: Vec::new(),
    };
    finder.visit_program(&program);
    let (queries, diagnostics) = (finder.queries, finder.diagnostics);
    Some(FileScan {
        queries,
        diagnostics,
        lines,
    })
}

/// A module's re-exports: what it exports that comes from another module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleExports {
    pub entries: Vec<ExportEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportEntry {
    /// `exported` is `name` of the module `spec` (`*` for the module
    /// itself): `export { name as exported } from 'spec'`, `export * as
    /// exported from 'spec'`, or an imported binding exported again.
    Named {
        exported: String,
        spec: String,
        name: String,
    },
    /// `export * from 'spec'`.
    All { spec: String },
}

/// The re-exports of the module `source` (the file at `path`); `None` when
/// it doesn't parse.
pub fn module_exports(path: &Path, source: &str) -> Option<ModuleExports> {
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.diagnostics.has_errors() {
        return None;
    }
    // Imported bindings, by local name: module scope, so a name suffices.
    let mut imports: HashMap<&str, (&str, &str)> = HashMap::new();
    for stmt in &parsed.program.body {
        if let Statement::ImportDeclaration(import) = stmt {
            for s in import.specifiers.iter().flatten() {
                match s {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        imports.insert(
                            s.local.name.as_str(),
                            (import.source.value.as_str(), export_name(&s.imported)),
                        );
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        imports.insert(s.local.name.as_str(), (import.source.value.as_str(), "*"));
                    }
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {}
                }
            }
        }
    }
    let mut entries = Vec::new();
    for stmt in &parsed.program.body {
        match stmt {
            Statement::ExportNamedDeclaration(export) if !export.export_kind.is_type() => {
                named_exports(&export.specifiers, None, &imports, &mut entries);
            }
            Statement::ExportFromDeclaration(export) if !export.export_kind.is_type() => {
                let spec = export.source.value.as_str();
                named_exports(&export.specifiers, Some(spec), &imports, &mut entries);
            }
            Statement::ExportAllDeclaration(export) if !export.export_kind.is_type() => {
                let spec = export.source.value.as_str().to_owned();
                entries.push(match &export.exported {
                    Some(name) => ExportEntry::Named {
                        exported: export_name(name).to_owned(),
                        spec,
                        name: "*".to_owned(),
                    },
                    None => ExportEntry::All { spec },
                });
            }
            _ => {}
        }
    }
    Some(ModuleExports { entries })
}

fn named_exports(
    specifiers: &[ExportSpecifier],
    source: Option<&str>,
    imports: &HashMap<&str, (&str, &str)>,
    entries: &mut Vec<ExportEntry>,
) {
    for s in specifiers {
        if s.export_kind.is_type() {
            continue;
        }
        let exported = export_name(&s.exported).to_owned();
        let local = export_name(&s.local);
        let (spec, name) = match source {
            Some(spec) => (spec, local),
            None => match imports.get(local) {
                Some(&(spec, name)) => (spec, name),
                None => continue,
            },
        };
        entries.push(ExportEntry::Named {
            exported,
            spec: spec.to_owned(),
            name: name.to_owned(),
        });
    }
}

#[derive(Debug, Clone)]
struct Binding {
    spec: String,
    /// The imported name; `*` for a namespace import.
    name: String,
}

fn export_name<'a>(name: &'a ModuleExportName<'a>) -> &'a str {
    match name {
        ModuleExportName::IdentifierName(n) => n.name.as_str(),
        ModuleExportName::IdentifierReference(n) => n.name.as_str(),
        ModuleExportName::StringLiteral(s) => s.value.as_str(),
    }
}

struct Finder<'s, 'r> {
    scoping: &'s Scoping,
    bindings: &'s HashMap<SymbolId, Binding>,
    resolve: &'r mut dyn FnMut(&str, &str) -> Option<Export>,
    /// `(specifier, name)` → what it is, resolved once per file.
    cache: HashMap<(String, String), Option<Export>>,
    lines: &'s LineIndex,
    queries: Vec<FoundQuery>,
    diagnostics: Vec<ScanDiagnostic>,
}

impl Finder<'_, '_> {
    fn resolve(&mut self, spec: &str, name: &str) -> Option<Export> {
        let key = (spec.to_owned(), name.to_owned());
        if let Some(r) = self.cache.get(&key) {
            return *r;
        }
        let r = (self.resolve)(spec, name);
        self.cache.insert(key, r);
        r
    }

    /// The import binding `callee` names, and the member called on it.
    fn callee_binding<'b>(&self, callee: &'b Expression) -> Option<(Binding, Option<&'b str>)> {
        let (id, member) = match callee.without_parentheses() {
            Expression::Identifier(id) => (id, None),
            Expression::StaticMemberExpression(m) => match m.object.without_parentheses() {
                Expression::Identifier(id) => (id, Some(m.property.name.as_str())),
                _ => return None,
            },
            _ => return None,
        };
        let symbol = self
            .scoping
            .get_reference(id.reference_id.get()?)
            .symbol_id()?;
        Some((self.bindings.get(&symbol)?.clone(), member))
    }

    /// Whether the call looks like one to typedpg by its names alone —
    /// enough to report a misuse without resolving every call's module.
    fn named_like_ours(binding: &Binding, member: Option<&str>) -> bool {
        Kind::of_export(member.unwrap_or(&binding.name)).is_some()
    }

    /// The generated function `binding` (`.member`) is, if it is one.
    fn target(&mut self, binding: &Binding, member: Option<&str>) -> Option<(usize, Kind)> {
        let item = |e: Option<Export>| match e? {
            Export::Item { db, kind } => Some((db, kind)),
            Export::Namespace { .. } => None,
        };
        match (binding.name == "*", member) {
            // `ns.sql(...)`: the module's own export.
            (true, Some(member)) => item(self.resolve(&binding.spec, member)),
            (true, None) => None,
            (false, None) => item(self.resolve(&binding.spec, &binding.name)),
            // `{ ns }` re-exported as a namespace, then `ns.sql(...)`.
            (false, Some(member)) => match self.resolve(&binding.spec, &binding.name)? {
                Export::Namespace { db } => Some((db, Kind::of_export(member)?)),
                Export::Item { .. } => None,
            },
        }
    }

    fn diagnostic(&mut self, offset: u32, message: impl Into<String>) {
        self.diagnostics.push(ScanDiagnostic {
            pos: self.lines.position(offset as usize),
            message: message.into(),
        });
    }
}

impl<'a> Visit<'a> for Finder<'_, '_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Some((binding, member)) = self.callee_binding(&call.callee) {
            let literal_arg = match call.arguments.as_slice() {
                [Argument::StringLiteral(_)] => true,
                [Argument::TemplateLiteral(t)] => t.expressions.is_empty(),
                _ => false,
            };
            // A literal argument: resolve whatever is called. Anything else
            // only when the names say it's ours, to report the misuse.
            let target = if literal_arg || Self::named_like_ours(&binding, member) {
                self.target(&binding, member)
            } else {
                None
            };
            if let Some((db, kind)) = target {
                self.record(call, db, kind);
            }
        }
        walk::walk_call_expression(self, call);
    }

    fn visit_tagged_template_expression(&mut self, it: &TaggedTemplateExpression<'a>) {
        if let Some((binding, member)) = self.callee_binding(&it.tag)
            && Self::named_like_ours(&binding, member)
            && let Some((_, kind)) = self.target(&binding, member)
        {
            let name = match kind {
                Kind::Query => SQL_EXPORT,
                Kind::CopyIn => COPY_IN_EXPORT,
            };
            self.diagnostic(
                it.span.start,
                format!("{name} is called as a function, not as a template tag: {name}(`...`)"),
            );
        }
        walk::walk_tagged_template_expression(self, it);
    }
}

impl Finder<'_, '_> {
    fn record(&mut self, call: &CallExpression, db: usize, kind: Kind) {
        let what = match kind {
            Kind::Query => "the query passed to sql()",
            Kind::CopyIn => "the target passed to copyIn()",
        };
        let literal = match call.arguments.as_slice() {
            [Argument::StringLiteral(s)] => Some((
                s.value.as_str().to_owned(),
                s.span.start,
                Literal {
                    content_start: s.span.start as usize + 1,
                    raw: s.raw.map_or_else(String::new, |r| {
                        let r = r.as_str();
                        r[1..r.len() - 1].to_owned()
                    }),
                    template: false,
                },
            )),
            [Argument::TemplateLiteral(t)] if t.expressions.is_empty() => {
                let quasi = &t.quasis[0];
                match &quasi.value.cooked {
                    Some(cooked) => Some((
                        cooked.as_str().to_owned(),
                        t.span.start,
                        Literal {
                            content_start: quasi.span.start as usize,
                            raw: quasi.value.raw.as_str().to_owned(),
                            template: true,
                        },
                    )),
                    None => {
                        self.diagnostic(t.span.start, "invalid escape sequence in the literal");
                        None
                    }
                }
            }
            [Argument::TemplateLiteral(t)] => {
                let message = match kind {
                    Kind::Query => {
                        "a query can't interpolate values: write `$name` in the SQL and pass \
                         the value in the parameters"
                    }
                    Kind::CopyIn => "a copyIn() target can't interpolate values",
                };
                self.diagnostic(t.span.start, message);
                None
            }
            [arg] => {
                self.diagnostic(
                    arg.span().start,
                    format!("{what} must be a string literal, for typedpg to analyze it"),
                );
                None
            }
            _ => {
                let name = match kind {
                    Kind::Query => SQL_EXPORT,
                    Kind::CopyIn => COPY_IN_EXPORT,
                };
                self.diagnostic(
                    call.span.start,
                    format!("{name}() takes exactly one argument: {what}"),
                );
                None
            }
        };
        if let Some((text, start, literal)) = literal {
            self.queries.push(FoundQuery {
                db,
                kind,
                text,
                pos: self.lines.position(start as usize),
                literal,
            });
        }
    }
}

/// The byte offset in `raw` (a literal's contents as written) of the
/// character at byte `cooked` of its value. Past the end maps to the end.
pub fn cooked_to_raw(raw: &str, template: bool, cooked: usize) -> usize {
    let bytes = raw.as_bytes();
    let mut i = 0;
    let mut produced = 0;
    while i < bytes.len() {
        if produced >= cooked {
            return i;
        }
        let rest = &raw[i..];
        let c = rest.chars().next().expect("in bounds");
        if c == '\\' {
            let (len, out) = escape(&raw[i + 1..]);
            i += 1 + len;
            produced += out;
        } else if template && c == '\r' {
            // A template's raw `\r\n` / `\r` is a `\n` in its value.
            i += if rest.starts_with("\r\n") { 2 } else { 1 };
            produced += 1;
        } else {
            i += c.len_utf8();
            produced += c.len_utf8();
        }
    }
    bytes.len()
}

/// An escape sequence (after its `\`): how many bytes it spans and how many
/// bytes of UTF-8 it decodes to.
fn escape(s: &str) -> (usize, usize) {
    let Some(c) = s.chars().next() else {
        return (0, 0);
    };
    let hex = |s: &str, n: usize| -> Option<u32> {
        s.get(..n)
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
    };
    let utf8_len = |cp: u32| char::from_u32(cp).map_or(3, char::len_utf8);
    match c {
        // Line continuations produce nothing.
        '\r' if s.starts_with("\r\n") => (2, 0),
        '\n' | '\r' | '\u{2028}' | '\u{2029}' => (c.len_utf8(), 0),
        'x' => match hex(&s[1..], 2) {
            Some(cp) => (3, utf8_len(cp)),
            None => (1, 1),
        },
        'u' if s[1..].starts_with('{') => match s[2..].find('}') {
            Some(end) => {
                let cp = u32::from_str_radix(&s[2..2 + end], 16).unwrap_or(0xFFFD);
                (3 + end, utf8_len(cp))
            }
            None => (1, 1),
        },
        'u' => match hex(&s[1..], 4) {
            // A surrogate pair (`😀`) is one 4-byte character.
            Some(hi @ 0xD800..=0xDBFF)
                if s[5..].starts_with("\\u")
                    && hex(&s[7..], 4).is_some_and(|lo| (0xDC00..=0xDFFF).contains(&lo)) =>
            {
                let _ = hi;
                (11, 4)
            }
            Some(cp) => (5, utf8_len(cp)),
            None => (1, 1),
        },
        c => (c.len_utf8(), c.len_utf8()),
    }
}

/// Byte offset → line / column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineIndex {
    starts: Vec<usize>,
    text: String,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex {
            starts,
            text: source.to_owned(),
        }
    }

    pub fn position(&self, offset: usize) -> Position {
        let offset = offset.min(self.text.len());
        let line = self.starts.partition_point(|&s| s <= offset) - 1;
        let start = self.starts[line];
        let col = self.text[start..offset].chars().count();
        Position {
            line: line as u32 + 1,
            col: col as u32 + 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scan `src` with `./db` (database 0) and `./other` (database 1) as the
    /// generated modules, and `./barrel` re-exporting `./db`'s `sql` as `q`
    /// and the module as `dbns`.
    fn run(src: &str) -> FileScan {
        scan(Path::new("a.ts"), src, &mut |spec, name| {
            let db = match spec {
                "./db" => 0,
                "./other" => 1,
                "./barrel" => {
                    return match name {
                        "q" => Some(Export::Item {
                            db: 0,
                            kind: Kind::Query,
                        }),
                        "dbns" => Some(Export::Namespace { db: 0 }),
                        _ => None,
                    };
                }
                _ => return None,
            };
            match name {
                "*" => Some(Export::Namespace { db }),
                n => Kind::of_export(n).map(|kind| Export::Item { db, kind }),
            }
        })
        .unwrap()
    }

    fn texts(scan: &FileScan) -> Vec<(usize, Kind, &str)> {
        scan.queries
            .iter()
            .map(|q| (q.db, q.kind, q.text.as_str()))
            .collect()
    }

    #[test]
    fn finds_named_aliased_and_namespace_imports() {
        let s = run(r#"
import { sql, copyIn } from './db';
import { sql as q } from './other';
import * as ns from './db';
sql('SELECT 1');
q(`SELECT 2`);
ns.sql("SELECT 3");
copyIn("users (id)");
ns.copyIn("users");
"#);
        assert_eq!(
            texts(&s),
            vec![
                (0, Kind::Query, "SELECT 1"),
                (1, Kind::Query, "SELECT 2"),
                (0, Kind::Query, "SELECT 3"),
                (0, Kind::CopyIn, "users (id)"),
                (0, Kind::CopyIn, "users"),
            ]
        );
        assert!(s.diagnostics.is_empty(), "{:?}", s.diagnostics);
        assert_eq!(s.queries[0].pos, Position { line: 5, col: 5 });
    }

    #[test]
    fn follows_what_the_resolver_says_a_barrel_exports() {
        let s = run(r#"
import { q, dbns } from './barrel';
q('SELECT 1');
dbns.sql('SELECT 2');
dbns.other('NOT OURS');
"#);
        assert_eq!(
            texts(&s),
            vec![(0, Kind::Query, "SELECT 1"), (0, Kind::Query, "SELECT 2")]
        );
    }

    #[test]
    fn ignores_shadowing_and_other_modules() {
        let s = run(r#"
import { sql } from './db';
import { sql as other } from 'elsewhere';
function f(sql: (s: string) => void) { sql('NOT A QUERY'); }
other('NOT EITHER');
sql('SELECT 1');
"#);
        assert_eq!(texts(&s), vec![(0, Kind::Query, "SELECT 1")]);
    }

    #[test]
    fn template_literal_values_are_cooked() {
        let s = run("import { sql } from './db';\nsql(`SELECT\n  'a\\tb'`);\n");
        assert_eq!(texts(&s), vec![(0, Kind::Query, "SELECT\n  'a\tb'")]);
    }

    #[test]
    fn non_literal_queries_are_reported() {
        let s = run(r#"
import { sql, copyIn } from './db';
const t = 'SELECT 1';
sql(t);
sql(`SELECT ${t}`);
sql`SELECT 1`;
copyIn(t);
sql();
"#);
        assert!(s.queries.is_empty());
        let msgs: Vec<_> = s.diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(msgs.len(), 5, "{msgs:?}");
        assert!(msgs[0].contains("the query passed to sql() must be a string literal"));
        assert!(msgs[1].contains("can't interpolate"));
        assert!(msgs[2].contains("not as a template tag"));
        assert!(msgs[3].contains("the target passed to copyIn() must be a string literal"));
        assert!(msgs[4].contains("sql() takes exactly one argument"));
    }

    #[test]
    fn type_only_imports_are_not_queries() {
        let s = run("import type { sql } from './db';\nimport { type sql as s } from './db';\n");
        assert!(s.queries.is_empty());
    }

    #[test]
    fn module_exports_lists_re_exports() {
        let src = r#"
import { sql as s, other } from './db';
import * as ns from './db';
export { s as query, other };
export { ns };
export { copyIn } from './db';
export * from './more';
export * as all from './db';
export type { Queries } from './db';
export const local = 1;
"#;
        let e = module_exports(Path::new("b.ts"), src).unwrap().entries;
        let named = |exported: &str, spec: &str, name: &str| ExportEntry::Named {
            exported: exported.into(),
            spec: spec.into(),
            name: name.into(),
        };
        assert_eq!(
            e,
            vec![
                named("query", "./db", "sql"),
                named("other", "./db", "other"),
                named("ns", "./db", "*"),
                named("copyIn", "./db", "copyIn"),
                ExportEntry::All {
                    spec: "./more".into()
                },
                named("all", "./db", "*"),
            ]
        );
    }

    #[test]
    fn offsets_map_through_escapes() {
        let check = |raw: &str, template: bool, cooked: &str| {
            // Every character of the value maps to where it is written.
            for (offset, c) in cooked.char_indices() {
                let at = cooked_to_raw(raw, template, offset);
                let written = &raw[at..];
                let direct = written.starts_with(c);
                let escaped = written.starts_with('\\');
                let crlf = template && c == '\n' && written.starts_with('\r');
                assert!(
                    direct || escaped || crlf,
                    "{raw:?}: offset {offset} ({c:?}) maps to {written:?}"
                );
            }
        };
        check(
            r"SELECT 'a\tb' é \u{1F600} 😀 x",
            false,
            "SELECT 'a\tb' é 😀 😀 x",
        );
        check("SELECT\\\n 1", false, "SELECT 1");
        check("a\r\nb\rc", true, "a\nb\nc");
        check(r"\x41é\\z", false, "Aé\\z");
        assert_eq!(cooked_to_raw(r"a\tb", false, 2), 3);
        assert_eq!(cooked_to_raw("abc", false, 10), 3);
    }

    #[test]
    fn error_offsets_land_in_the_file() {
        let src = "import { sql } from './db';\nsql(`SELECT\n  \\u0061, nmae`);\n";
        let s = run(src);
        let q = &s.queries[0];
        let at = q.literal.source_offset(q.text.find("nmae").unwrap());
        assert_eq!(&src[at..at + 4], "nmae");
        assert_eq!(s.lines.position(at), Position { line: 3, col: 11 });
    }
}
