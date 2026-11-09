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
//!
//! The scan also records how a query is called: the methods on the `sql(…)`
//! expression, on a local `const`/`let` binding of it, and on a `.sql` query
//! or an exported `sql()` binding imported from another file. Those calls
//! are placed at their method identifier, in the file that makes them.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_ast::ast::{
    Argument, BindingPattern, CallExpression, Declaration, ExportSpecifier, Expression,
    ImportDeclarationSpecifier, ModuleExportName, Program, Statement, StaticMemberExpression,
    TaggedTemplateExpression, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::{AstNodes, Scoping, SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, SourceType, Span};

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

/// A query another module exports: a `.sql` file's query, or a TypeScript
/// source's `export const x = sql(…)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueryExport {
    /// The file that declares the query (canonical when the resolver reported
    /// it).
    pub file: PathBuf,
    /// The name it is exported as.
    pub name: String,
}

/// What an imported name is, as far as typedpg is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Export {
    /// A generated module's `sql` or `copyIn`.
    Item { db: usize, kind: Kind },
    /// A generated module itself, as a namespace (`import * as db`).
    Namespace { db: usize },
    /// A query another module exports.
    Query(QueryExport),
}

/// A method called on a query result in a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundCall {
    /// The method as written: `fetchOne`, `fetchValue`, `execute`, …
    pub method: String,
    /// Byte range of the method identifier.
    pub span: Range<usize>,
}

/// A call on a query another module declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCall {
    pub query: QueryExport,
    pub method: String,
    pub span: Range<usize>,
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
    /// The methods called on the query expression or a local binding of it,
    /// in source order.
    pub calls: Vec<FoundCall>,
    /// How many uses of the expression or a local binding the scan saw,
    /// method calls included.
    pub uses: usize,
    /// A use that is not a seen method call (`f(q)`, a property read, an
    /// assignment, …): `unused_returning` stays quiet then.
    pub escapes: bool,
    /// The names the query is exported as from this file.
    pub exports: Vec<String>,
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
    /// The quote the literal is written with: `'`, `"` or a template's
    /// backtick.
    pub quote: char,
}

impl Literal {
    /// The byte offset in the file of the character at byte `offset` of
    /// the literal's value.
    pub fn source_offset(&self, offset: usize) -> usize {
        self.content_start + cooked_to_raw(&self.raw, self.template, offset)
    }

    /// `text` escaped so that writing it into this literal gives back
    /// `text`: a template literal escapes `\`, a backtick and `${`; a quoted
    /// string escapes `\`, its quote and line breaks. `None` when the
    /// literal is neither a template nor a quoted string.
    pub fn escape_replacement(&self, text: &str) -> Option<String> {
        let mut out = String::with_capacity(text.len());
        if self.template {
            let mut chars = text.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '`' => out.push_str("\\`"),
                    '\r' => out.push_str("\\r"),
                    '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
                    c => out.push(c),
                }
            }
        } else if self.quote == '\'' || self.quote == '"' {
            for c in text.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    c if c == self.quote => {
                        out.push('\\');
                        out.push(c);
                    }
                    c => out.push(c),
                }
            }
        } else {
            return None;
        }
        Some(out)
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
    /// Calls on queries declared in other files.
    pub remote_calls: Vec<RemoteCall>,
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
                    let Some(name) = export_name(&s.imported) else {
                        continue;
                    };
                    (&s.local, name.to_owned())
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                    (&s.local, "*".to_owned())
                }
                _ => continue,
            };
            // A specifier with a lone surrogate names no file.
            let Some(spec) = import.source.value.as_str() else {
                continue;
            };
            imported.push((local, spec.to_owned(), name));
        }
    }
    if imported.is_empty() {
        return Some(FileScan {
            lines,
            ..FileScan::default()
        });
    }

    // Symbols are assigned by the semantic pass.
    let semantic = SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&program)
        .semantic;
    let bindings: HashMap<SymbolId, Binding> = imported
        .into_iter()
        .filter_map(|(local, spec, name)| Some((local.symbol_id.get()?, Binding { spec, name })))
        .collect();

    let mut finder = Finder {
        scoping: semantic.scoping(),
        nodes: semantic.nodes(),
        bindings: &bindings,
        resolve,
        cache: HashMap::new(),
        lines: &lines,
        queries: Vec::new(),
        remote_calls: Vec::new(),
        query_bindings: HashMap::new(),
        recorded: HashMap::new(),
        handled: std::collections::HashSet::new(),
        exporting: false,
        diagnostics: Vec::new(),
    };
    finder.visit_program(&program);
    finder.analyze_bindings();
    finder.record_local_exports(&program);
    Some(FileScan {
        queries: finder.queries,
        remote_calls: finder.remote_calls,
        diagnostics: finder.diagnostics,
        lines,
    })
}

/// A module's re-exports: what it exports that comes from another module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleExports {
    pub entries: Vec<ExportEntry>,
    /// The names of the queries the module declares and exports itself
    /// (`export const x = sql(…)`, or a local one re-exported).
    pub queries: Vec<String>,
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
        if let Statement::ImportDeclaration(import) = stmt
            && let Some(spec) = import.source.value.as_str()
        {
            for s in import.specifiers.iter().flatten() {
                match s {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        if let Some(name) = export_name(&s.imported) {
                            imports.insert(s.local.name.as_str(), (spec, name));
                        }
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        imports.insert(s.local.name.as_str(), (spec, "*"));
                    }
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {}
                }
            }
        }
    }
    // The queries the module declares itself: a local `const x = sql(…)`
    // (exported with `export`, or by a later `export { x }`).
    let mut local_queries = HashSet::new();
    for stmt in &parsed.program.body {
        let variables = match stmt {
            Statement::VariableDeclaration(variables) => variables,
            Statement::ExportDeclaration(export) => match &export.declaration {
                Declaration::VariableDeclaration(variables) => variables,
                _ => continue,
            },
            _ => continue,
        };
        for declarator in &variables.declarations {
            if let Some(name) = declared_query(declarator, &imports) {
                local_queries.insert(name.to_owned());
            }
        }
    }
    let mut entries = Vec::new();
    let mut queries = Vec::new();
    for stmt in &parsed.program.body {
        match stmt {
            Statement::ExportDeclaration(export) => {
                if let Declaration::VariableDeclaration(variables) = &export.declaration {
                    for declarator in &variables.declarations {
                        if let Some(name) = declared_query(declarator, &imports) {
                            queries.push(name.to_owned());
                        }
                    }
                }
            }
            Statement::ExportNamedDeclaration(export) if !export.export_kind.is_type() => {
                named_exports(&export.specifiers, None, &imports, &mut entries);
                for specifier in &export.specifiers {
                    let (Some(local), Some(exported)) = (
                        export_name(&specifier.local),
                        export_name(&specifier.exported),
                    ) else {
                        continue;
                    };
                    if local_queries.contains(local) {
                        queries.push(exported.to_owned());
                    }
                }
            }
            Statement::ExportFromDeclaration(export) if !export.export_kind.is_type() => {
                if let Some(spec) = export.source.value.as_str() {
                    named_exports(&export.specifiers, Some(spec), &imports, &mut entries);
                }
            }
            Statement::ExportAllDeclaration(export) if !export.export_kind.is_type() => {
                let Some(spec) = export.source.value.as_str() else {
                    continue;
                };
                let spec = spec.to_owned();
                entries.push(match &export.exported {
                    Some(name) => match export_name(name) {
                        Some(exported) => ExportEntry::Named {
                            exported: exported.to_owned(),
                            spec,
                            name: "*".to_owned(),
                        },
                        None => continue,
                    },
                    None => ExportEntry::All { spec },
                });
            }
            _ => {}
        }
    }
    queries.dedup();
    Some(ModuleExports { entries, queries })
}

/// The name a `const x = sql(…)` declarator binds, when its initializer is a
/// `sql()` call.
fn declared_query<'a>(
    declarator: &'a VariableDeclarator<'a>,
    imports: &HashMap<&str, (&str, &str)>,
) -> Option<&'a str> {
    let BindingPattern::BindingIdentifier(id) = &declarator.id else {
        return None;
    };
    let init = declarator.init.as_ref()?;
    is_sql_call(init, imports).then(|| id.name.as_str())
}

/// Whether `expr` is a `sql(…)` call, with `sql` imported or a generated
/// module's namespace member. This is the syntax-only check the export scan
/// can do without the resolver; the full scan resolves the import.
fn is_sql_call(expr: &Expression, imports: &HashMap<&str, (&str, &str)>) -> bool {
    let Expression::CallExpression(call) = expr.without_parentheses() else {
        return false;
    };
    match call.callee.without_parentheses() {
        Expression::Identifier(id) => {
            id.name == SQL_EXPORT
                || imports
                    .get(id.name.as_str())
                    .is_some_and(|&(_, name)| name == SQL_EXPORT)
        }
        Expression::StaticMemberExpression(member) => {
            member.property.name == SQL_EXPORT
                && matches!(
                    member.object.without_parentheses(),
                    Expression::Identifier(id)
                        if imports
                            .get(id.name.as_str())
                            .is_some_and(|&(_, name)| name == "*")
                )
        }
        _ => false,
    }
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
        let (Some(exported), Some(local)) = (export_name(&s.exported), export_name(&s.local))
        else {
            continue;
        };
        let exported = exported.to_owned();
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

/// The method called on `callee`, when it is a static member call
/// (`q.fetchOne(…)`).
fn member_call<'a>(
    callee: &'a Expression<'a>,
) -> Option<(&'a str, &'a StaticMemberExpression<'a>)> {
    let Expression::StaticMemberExpression(member) = callee.without_parentheses() else {
        return None;
    };
    Some((member.property.name.as_str(), member))
}

/// Whether `name` is a method the call-site rules map to a [`CallMethod`].
fn mapped_method(name: &str) -> Option<()> {
    matches!(
        name,
        "fetchOne" | "fetchValue" | "fetchOptional" | "fetchValueOptional" | "fetchAll" | "execute"
    )
    .then_some(())
}

#[derive(Debug, Clone)]
struct Binding {
    spec: String,
    /// The imported name; `*` for a namespace import.
    name: String,
}

/// `None` for a string name with a lone surrogate, which the spec rejects
/// (`IsStringWellFormedUnicode`): no import or export can use it.
fn export_name<'a>(name: &'a ModuleExportName<'a>) -> Option<&'a str> {
    match name {
        ModuleExportName::IdentifierName(n) => Some(n.name.as_str()),
        ModuleExportName::IdentifierReference(n) => Some(n.name.as_str()),
        ModuleExportName::StringLiteral(s) => s.value.as_str(),
    }
}

struct Finder<'s, 'r> {
    scoping: &'s Scoping,
    nodes: &'s AstNodes<'s>,
    bindings: &'s HashMap<SymbolId, Binding>,
    resolve: &'r mut dyn FnMut(&str, &str) -> Option<Export>,
    /// `(specifier, name)` → what it is, resolved once per file.
    cache: HashMap<(String, String), Option<Export>>,
    lines: &'s LineIndex,
    queries: Vec<FoundQuery>,
    remote_calls: Vec<RemoteCall>,
    /// The queries a local `const q = sql(…)` binds: symbol → query index.
    query_bindings: HashMap<SymbolId, usize>,
    /// The byte start of each `sql()` / `copyIn()` call already turned into a
    /// query, and the query it is.
    recorded: HashMap<u32, usize>,
    /// The calls already examined (recorded or rejected), so walking a call
    /// twice does not report its misuse twice.
    handled: HashSet<u32>,
    /// Whether the declaration being visited is `export const …`.
    exporting: bool,
    diagnostics: Vec<ScanDiagnostic>,
}

impl Finder<'_, '_> {
    fn resolve(&mut self, spec: &str, name: &str) -> Option<Export> {
        let key = (spec.to_owned(), name.to_owned());
        if let Some(r) = self.cache.get(&key) {
            return r.clone();
        }
        let r = (self.resolve)(spec, name);
        self.cache.insert(key, r.clone());
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
            Export::Namespace { .. } | Export::Query(_) => None,
        };
        match (binding.name == "*", member) {
            // `ns.sql(...)`: the module's own export.
            (true, Some(member)) => item(self.resolve(&binding.spec, member)),
            (true, None) => None,
            (false, None) => item(self.resolve(&binding.spec, &binding.name)),
            // `{ ns }` re-exported as a namespace, then `ns.sql(...)`.
            (false, Some(member)) => match self.resolve(&binding.spec, &binding.name)? {
                Export::Namespace { db } => Some((db, Kind::of_export(member)?)),
                Export::Item { .. } | Export::Query(_) => None,
            },
        }
    }

    /// The query `binding` (`.member`) is, if it is another module's.
    fn query_export(&mut self, binding: &Binding, member: Option<&str>) -> Option<QueryExport> {
        let query = |e: Option<Export>| match e? {
            Export::Query(query) => Some(query),
            Export::Item { .. } | Export::Namespace { .. } => None,
        };
        match (binding.name == "*", member) {
            // `ns.userById`: a namespace import's query export.
            (true, Some(member)) => query(self.resolve(&binding.spec, member)),
            (true, None) => None,
            // `userById` or `userById as id`: a named import.
            (false, None) => query(self.resolve(&binding.spec, &binding.name)),
            (false, Some(_)) => None,
        }
    }

    /// The query `expr` names, when it is an imported binding: a named
    /// import used directly, or `ns.name` through a namespace import.
    fn imported_query(&mut self, expr: &Expression) -> Option<QueryExport> {
        match expr.without_parentheses() {
            Expression::Identifier(id) => {
                let symbol = self
                    .scoping
                    .get_reference(id.reference_id.get()?)
                    .symbol_id()?;
                let binding = self.bindings.get(&symbol)?.clone();
                if binding.name == "*" {
                    return None;
                }
                self.query_export(&binding, None)
            }
            Expression::StaticMemberExpression(member) => {
                let Expression::Identifier(id) = member.object.without_parentheses() else {
                    return None;
                };
                let symbol = self
                    .scoping
                    .get_reference(id.reference_id.get()?)
                    .symbol_id()?;
                let binding = self.bindings.get(&symbol)?.clone();
                if binding.name != "*" {
                    return None;
                }
                self.query_export(&binding, Some(member.property.name.as_str()))
            }
            _ => None,
        }
    }

    /// The generated `sql()` call `expr` is, when it is the receiver of a
    /// method call: `sql("…").fetchOne(…)`.
    fn query_call(&mut self, expr: &Expression) -> Option<(usize, Kind)> {
        let Expression::CallExpression(call) = expr.without_parentheses() else {
            return None;
        };
        let (binding, member) = self.callee_binding(&call.callee)?;
        let (db, kind) = self.target(&binding, member)?;
        (kind == Kind::Query).then_some((db, kind))
    }

    /// Record `call` as the query it is, once: the walk visits a receiver
    /// `sql()` call after the outer method call that already recorded it.
    fn record_once(&mut self, call: &CallExpression, db: usize, kind: Kind) -> Option<usize> {
        let start = call.span.start;
        if let Some(&index) = self.recorded.get(&start) {
            return Some(index);
        }
        if !self.handled.insert(start) {
            return None;
        }
        let before = self.queries.len();
        self.record(call, db, kind);
        if self.queries.len() > before {
            self.recorded.insert(start, before);
            Some(before)
        } else {
            None
        }
    }

    /// [`Self::record_once`] of the `sql()` call `receiver` is.
    fn record_receiver(&mut self, receiver: &Expression, db: usize, kind: Kind) -> Option<usize> {
        let Expression::CallExpression(call) = receiver.without_parentheses() else {
            return None;
        };
        self.record_once(call, db, kind)
    }

    /// Attach a method call to query `index`.
    fn attach(&mut self, index: usize, method: &str, span: Span) {
        let query = &mut self.queries[index];
        query.calls.push(FoundCall {
            method: method.to_owned(),
            span: span.start as usize..span.end as usize,
        });
        query.uses += 1;
        // `fetchStream` (and anything not mapped to a rule) is still a use:
        // `unused_returning` cannot be sure every use is an `execute()`.
        if mapped_method(method).is_none() {
            query.escapes = true;
        }
    }

    /// The local `const q = sql(…)` a declarator is, recorded and mapped to
    /// its symbol so the semantic references can be classified later.
    fn local_query(&mut self, declarator: &VariableDeclarator) -> Option<usize> {
        let BindingPattern::BindingIdentifier(id) = &declarator.id else {
            return None;
        };
        let init = declarator.init.as_ref()?;
        let Expression::CallExpression(call) = init.without_parentheses() else {
            return None;
        };
        let (binding, member) = self.callee_binding(&call.callee)?;
        let (db, kind) = self.target(&binding, member)?;
        if kind != Kind::Query {
            return None;
        }
        let index = self.record_once(call, db, kind)?;
        let symbol = id.symbol_id.get()?;
        self.query_bindings.insert(symbol, index);
        Some(index)
    }

    /// Classify every reference to a local query binding: method calls are
    /// call sites, anything else means the query escapes.
    fn analyze_bindings(&mut self) {
        let mut bindings: Vec<(SymbolId, usize)> = self
            .query_bindings
            .iter()
            .map(|(&symbol, &index)| (symbol, index))
            .collect();
        bindings.sort_by_key(|(_, index)| *index);
        for (symbol, index) in bindings {
            let mut references: Vec<_> = self
                .scoping
                .get_resolved_reference_ids(symbol)
                .iter()
                .map(|&reference| self.scoping.get_reference(reference))
                .collect();
            references
                .sort_by_key(|reference| self.nodes.get_node(reference.node_id()).span().start);
            let mut uses = 0usize;
            let mut escapes = false;
            for reference in references {
                let node = self.nodes.get_node(reference.node_id());
                if reference.is_write() {
                    // An assignment replaces the query: later calls are on
                    // another value.
                    uses += 1;
                    escapes = true;
                    break;
                }
                if let Some((method, span)) = self.method_call_at(node) {
                    uses += 1;
                    self.queries[index].calls.push(FoundCall {
                        method: method.clone(),
                        span: span.start as usize..span.end as usize,
                    });
                    if mapped_method(&method).is_none() {
                        escapes = true;
                    }
                } else {
                    uses += 1;
                    escapes = true;
                }
            }
            self.queries[index].uses += uses;
            self.queries[index].escapes |= escapes;
        }
    }

    /// The `name` of the `X.name(…)` call `node` is the receiver of, when
    /// `node` is an identifier reference directly.
    fn method_call_at(&self, node: &oxc_semantic::AstNode<'_>) -> Option<(String, Span)> {
        if !matches!(node.kind(), AstKind::IdentifierReference(_)) {
            return None;
        }
        let parent = self.nodes.parent_node(node.id());
        let AstKind::StaticMemberExpression(member) = parent.kind() else {
            return None;
        };
        if member.object.span() != node.span() {
            return None;
        }
        let call = self.nodes.parent_node(parent.id());
        let AstKind::CallExpression(call) = call.kind() else {
            return None;
        };
        if call.callee.without_parentheses().span() != member.span {
            return None;
        }
        Some((member.property.name.to_string(), member.property.span))
    }

    /// A local query exported by `export { q as name }`.
    fn record_local_exports(&mut self, program: &Program<'_>) {
        for statement in &program.body {
            let Statement::ExportNamedDeclaration(export) = statement else {
                continue;
            };
            if export.export_kind.is_type() {
                continue;
            }
            for specifier in &export.specifiers {
                if specifier.export_kind.is_type() {
                    continue;
                }
                let ModuleExportName::IdentifierReference(local) = &specifier.local else {
                    continue;
                };
                let Some(reference) = local.reference_id.get() else {
                    continue;
                };
                let Some(symbol) = self.scoping.get_reference(reference).symbol_id() else {
                    continue;
                };
                let Some(&index) = self.query_bindings.get(&symbol) else {
                    continue;
                };
                let Some(name) = export_name(&specifier.exported) else {
                    continue;
                };
                if !self.queries[index].exports.iter().any(|e| e == name) {
                    self.queries[index].exports.push(name.to_owned());
                }
                self.queries[index].escapes = true;
            }
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
    fn visit_variable_declarator(&mut self, declarator: &VariableDeclarator<'a>) {
        if let Some(index) = self.local_query(declarator)
            && self.exporting
            && let BindingPattern::BindingIdentifier(id) = &declarator.id
        {
            self.queries[index].exports.push(id.name.to_string());
            self.queries[index].escapes = true;
        }
        walk::walk_variable_declarator(self, declarator);
    }

    fn visit_export_declaration(&mut self, declaration: &oxc_ast::ast::ExportDeclaration<'a>) {
        let exporting = self.exporting;
        self.exporting = true;
        walk::walk_export_declaration(self, declaration);
        self.exporting = exporting;
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        // The call itself may be a `sql(…)` / `copyIn(…)`.
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
                self.record_once(call, db, kind);
            }
        }
        // A method called on a query expression: `sql("…").fetchOne(…)`, or
        // on another module's imported query.
        if let Some((method, member)) = member_call(&call.callee) {
            if let Some((db, kind)) = self.query_call(&member.object) {
                if let Some(index) = self.record_receiver(&member.object, db, kind) {
                    self.attach(index, method, member.property.span);
                }
            } else if let Some(query) = self.imported_query(&member.object) {
                self.remote_calls.push(RemoteCall {
                    query,
                    method: method.to_owned(),
                    span: member.property.span.start as usize..member.property.span.end as usize,
                });
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

/// A literal whose value has a lone surrogate (an unpaired `\uD800` to
/// `\uDFFF` escape): not UTF-8, which is all PostgreSQL takes — Node would
/// send U+FFFD in its place, another query than the one written.
const LONE_SURROGATE: &str = "the literal has a lone surrogate (an unpaired \\uD800 to \\uDFFF escape): PostgreSQL \
     takes only UTF-8";

impl Finder<'_, '_> {
    fn record(&mut self, call: &CallExpression, db: usize, kind: Kind) {
        let what = match kind {
            Kind::Query => "the query passed to sql()",
            Kind::CopyIn => "the target passed to copyIn()",
        };
        let literal = match call.arguments.as_slice() {
            [Argument::StringLiteral(s)] if s.value.as_str().is_none() => {
                self.diagnostic(s.span.start, LONE_SURROGATE);
                None
            }
            [Argument::StringLiteral(s)] => {
                let raw = s.raw.map_or_else(String::new, |r| r.as_str().to_owned());
                let quote = raw.chars().next().unwrap_or('"');
                Some((
                    s.value.as_str().unwrap_or_default().to_owned(),
                    s.span.start,
                    Literal {
                        content_start: s.span.start as usize + 1,
                        raw: raw
                            .get(1..raw.len().saturating_sub(1))
                            .unwrap_or_default()
                            .to_owned(),
                        template: false,
                        quote,
                    },
                ))
            }
            [Argument::TemplateLiteral(t)] if t.expressions.is_empty() => {
                let quasi = &t.quasis[0];
                match quasi.value.cooked.map(|c| c.as_str()) {
                    Some(None) => {
                        self.diagnostic(t.span.start, LONE_SURROGATE);
                        None
                    }
                    Some(Some(cooked)) => Some((
                        cooked.to_owned(),
                        t.span.start,
                        Literal {
                            content_start: quasi.span.start as usize,
                            raw: quasi.value.raw.as_str().to_owned(),
                            template: true,
                            quote: '`',
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
                calls: Vec::new(),
                uses: 0,
                escapes: false,
                exports: Vec::new(),
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
        // `\uXXXX` or `\u{…}`: a high surrogate followed at once by a low
        // one, in either spelling, is one 4-byte character.
        'u' => match unicode_escape(s) {
            Some((len, hi @ 0xD800..=0xDBFF)) => {
                // Line continuations between the halves produce nothing.
                let gap = line_continuations(&s[len..]);
                match s[len + gap..].strip_prefix('\\').and_then(unicode_escape) {
                    Some((lo_len, 0xDC00..=0xDFFF)) => (len + gap + 1 + lo_len, 4),
                    _ => (len, utf8_len(hi)),
                }
            }
            Some((len, cp)) => (len, utf8_len(cp)),
            None => (1, 1),
        },
        c => (c.len_utf8(), c.len_utf8()),
    }
}

/// How many bytes of line continuations (`\` and a line terminator) `s`
/// starts with.
fn line_continuations(s: &str) -> usize {
    let mut at = 0;
    while let Some(rest) = s[at..].strip_prefix('\\') {
        let len = if rest.starts_with("\r\n") {
            2
        } else {
            match rest.chars().next() {
                Some(c @ ('\n' | '\r' | '\u{2028}' | '\u{2029}')) => c.len_utf8(),
                _ => break,
            }
        };
        at += 1 + len;
    }
    at
}

/// A `\u` escape at the start of `s` (after its backslash, so `s` starts
/// with `u`): how many bytes it spans and the code unit or point it names.
fn unicode_escape(s: &str) -> Option<(usize, u32)> {
    let rest = s.strip_prefix('u')?;
    if let Some(braced) = rest.strip_prefix('{') {
        let end = braced.find('}')?;
        let cp = u32::from_str_radix(&braced[..end], 16).unwrap_or(0xFFFD);
        return Some((3 + end, cp));
    }
    let digits = rest.get(..4)?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((5, u32::from_str_radix(digits, 16).ok()?))
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

    /// The text the index was built from.
    pub fn text(&self) -> &str {
        &self.text
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
    fn surrogate_pairs_map_as_one_character_in_every_spelling() {
        // Each spelling of 😀 cooks to four bytes; the next byte is ` AS`.
        for raw in [
            r"'\u{D83D}\u{DE00}' AS emoji",
            r"'\uD83D\uDE00' AS emoji",
            r"'\u{D83D}\uDE00' AS emoji",
            r"'\uD83D\u{DE00}' AS emoji",
            r"'\u{1F600}' AS emoji",
            "'\\u{D83D}\\\n\\u{DE00}' AS emoji",
            "'\\uD83D\\\r\n\\\n\\uDE00' AS emoji",
        ] {
            let cooked = "'😀' AS emoji";
            let at = cooked.find(" AS").unwrap();
            let mapped = cooked_to_raw(raw, false, at);
            assert_eq!(&raw[mapped..], " AS emoji", "{raw}");
        }
    }

    #[test]
    fn lone_surrogates_are_reported() {
        // A pair is one character; an unpaired half is not UTF-8.
        let s = run(r#"
import { sql } from './db';
sql('SELECT \uD83D\uDE00');
sql('SELECT \uD800');
sql(`SELECT \uDC00`);
"#);
        assert_eq!(texts(&s), vec![(0, Kind::Query, "SELECT 😀")]);
        let msgs: Vec<_> = s.diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert!(
            msgs.iter().all(|m| m.contains("lone surrogate")),
            "{msgs:?}"
        );
        assert_eq!(s.diagnostics[0].pos, Position { line: 4, col: 5 });
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
    fn calls_on_a_direct_query_and_a_local_binding_are_recorded() {
        let s = run(r#"
import { sql } from './db';
sql('SELECT 1').fetchOne(pool);
const q = sql('SELECT 2');
q.fetchAll(pool);
q.execute(pool);
"#);
        assert_eq!(s.queries.len(), 2, "{:?}", s.queries);
        assert_eq!(s.queries[0].calls.len(), 1);
        assert_eq!(s.queries[0].calls[0].method, "fetchOne");
        assert_eq!(s.queries[0].uses, 1);
        assert!(!s.queries[0].escapes);
        let methods: Vec<&str> = s.queries[1]
            .calls
            .iter()
            .map(|call| call.method.as_str())
            .collect();
        assert_eq!(methods, ["fetchAll", "execute"]);
        assert_eq!(s.queries[1].uses, 2);
        assert!(!s.queries[1].escapes);
    }

    #[test]
    fn scopes_shadowing_and_reassignment_follow_the_semantic_references() {
        let s = run(r#"
import { sql } from './db';
const q = sql('SELECT 1');
q.fetchOne(pool);
{
    const q = sql('SELECT 2');
    q.execute(pool);
}
q.fetchOptional(pool);
q = other;
q.fetchAll(pool);
"#);
        assert_eq!(s.queries.len(), 2, "{:?}", s.queries);
        let outer: Vec<&str> = s.queries[0]
            .calls
            .iter()
            .map(|call| call.method.as_str())
            .collect();
        assert_eq!(outer, ["fetchOne", "fetchOptional"], "{:?}", s.queries[0]);
        assert!(s.queries[0].escapes);
        let inner: Vec<&str> = s.queries[1]
            .calls
            .iter()
            .map(|call| call.method.as_str())
            .collect();
        assert_eq!(inner, ["execute"]);
        assert!(!s.queries[1].escapes);
    }

    #[test]
    fn an_escaping_binding_keeps_the_calls_and_marks_the_escape() {
        let s = run(r#"
import { sql } from './db';
const q = sql('SELECT 1');
q.fetchOne(pool);
consume(q);
"#);
        assert_eq!(s.queries.len(), 1);
        assert_eq!(s.queries[0].calls.len(), 1);
        assert_eq!(s.queries[0].uses, 2);
        assert!(s.queries[0].escapes);
    }

    #[test]
    fn exported_queries_record_their_export_names() {
        let s = run(r#"
import { sql } from './db';
export const named = sql('SELECT 1');
const local = sql('SELECT 2');
export { local as renamed, local };
"#);
        assert_eq!(s.queries.len(), 2, "{:?}", s.queries);
        assert_eq!(s.queries[0].exports, ["named"]);
        assert!(s.queries[0].escapes);
        assert_eq!(s.queries[1].exports, ["renamed", "local"]);
    }

    #[test]
    fn calls_on_an_imported_query_are_remote_calls() {
        let export = |file: &str, name: &str| {
            Some(Export::Query(QueryExport {
                file: PathBuf::from(file),
                name: name.to_owned(),
            }))
        };
        let mut resolve = |spec: &str, name: &str| match (spec, name) {
            ("./queries", "userById") => export("/p/queries.ts", "userById"),
            ("./queries.sql", "userById") => export("/p/queries.sql", "userById"),
            _ => None,
        };
        let src = r#"
import { userById } from './queries';
import { userById as byId } from './queries.sql';
import * as queries from './queries';
userById.fetchOne(pool);
byId.fetchOptional(pool);
queries.userById.fetchAll(pool);
"#;
        let s = scan(Path::new("a.ts"), src, &mut resolve).unwrap();
        assert!(s.queries.is_empty());
        let calls: Vec<(&str, &str)> = s
            .remote_calls
            .iter()
            .map(|call| (call.query.name.as_str(), call.method.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("userById", "fetchOne"),
                ("userById", "fetchOptional"),
                ("userById", "fetchAll")
            ]
        );
        assert_eq!(
            s.remote_calls
                .iter()
                .map(|call| &call.query.file)
                .collect::<Vec<_>>(),
            [
                &PathBuf::from("/p/queries.ts"),
                &PathBuf::from("/p/queries.sql"),
                &PathBuf::from("/p/queries.ts")
            ]
        );
    }

    #[test]
    fn module_exports_include_exported_queries_and_their_aliases() {
        let src = r#"
import { sql } from './db';
export const named = sql('SELECT 1');
const local = sql('SELECT 2');
export { local as renamed };
export { userById } from './queries';
"#;
        let exports = module_exports(Path::new("a.ts"), src).unwrap();
        assert_eq!(exports.queries, ["named", "renamed"]);
        assert_eq!(exports.entries.len(), 1);
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

    #[test]
    fn a_fix_replacement_is_escaped_for_its_literal() {
        let s = run("import { sql } from './db';\nsql(`SELECT 1`);\n");
        assert_eq!(
            s.queries[0]
                .literal
                .escape_replacement("a`b${c}\\d")
                .unwrap(),
            "a\\`b\\${c}\\\\d"
        );
        let s = run("import { sql } from './db';\nsql('SELECT 1');\n");
        assert_eq!(
            s.queries[0]
                .literal
                .escape_replacement("a'b\\c\nd")
                .unwrap(),
            "a\\'b\\\\c\\nd"
        );
        let s = run("import { sql } from './db';\nsql(\"SELECT 1\");\n");
        assert_eq!(
            s.queries[0]
                .literal
                .escape_replacement("say \"hi\"")
                .unwrap(),
            "say \\\"hi\\\""
        );
    }
}
