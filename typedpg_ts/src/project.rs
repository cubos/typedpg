//! The state a run keeps: each source's queries, each `.sql` file's,
//! each database's catalog and analyzed queries. `gen` builds it once;
//! `--watch` keeps it and updates only what changed — a source rescans just
//! that file, a query already analyzed is not analyzed again, and a
//! generated module is rewritten only when its content changes.

use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};
use typedpg_analyzer::PgCatalog;
use typedpg_core::QualifiedName;

use crate::config::{
    Config, DatabaseConfig, Emit, OutKind, OverrideModule, declaration_path, out_kind,
};
use crate::emit::{Embedded, Module, TypeCheck};
use crate::generate::{GenError, Generated, generate};
use crate::scan::{Export, FileScan, Kind, ModuleExports, Position, module_exports, scan};
use crate::sqlfile::{SqlFile, parse as parse_sql};
use crate::sqlmodule::{Mapped, SqlModule, SqlQuery, generated_source, source_from};
use crate::typemap::{RUNTIME_NS, TypeMapper};

/// A problem to report, at a place in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub pos: Option<Position>,
    pub message: String,
}

impl Diagnostic {
    /// `file:line:col: error: message`, the path relative to `root`.
    pub fn render(&self, root: &Path) -> String {
        let file = self.file.strip_prefix(root).unwrap_or(&self.file).display();
        match self.pos {
            Some(p) => format!("{file}:{}:{}: error: {}", p.line, p.col, self.message),
            None => format!("{file}: error: {}", self.message),
        }
    }
}

/// What a [`Project::sync`] did.
#[derive(Debug, Default)]
pub struct SyncReport {
    /// Generated files whose content changed (and were written, unless
    /// checking).
    pub changed: Vec<PathBuf>,
    /// Modules generated for `.sql` files that are gone (deleted, unless
    /// checking).
    pub removed: Vec<PathBuf>,
    /// Queries analyzed in this sync (cache misses).
    pub analyzed: usize,
    pub diagnostics: Vec<Diagnostic>,
}

pub struct Project {
    pub config: Config,
    dbs: Vec<Database>,
    resolver: Resolver,
    files: BTreeMap<PathBuf, FileScan>,
    /// The re-exports of each module a source imports from, parsed once.
    modules: RefCell<HashMap<PathBuf, Option<ModuleExports>>>,
    /// Whether TypeScript sources are scanned (not to compile `.sql` files
    /// alone).
    scan_sources: bool,
    /// Each `.sql` file: its database and its queries.
    sql_files: BTreeMap<PathBuf, (usize, SqlFile)>,
    /// Each module generated for a `.sql` file → the file.
    sql_outputs: BTreeMap<PathBuf, PathBuf>,
    /// Files that are (or were written as) modules generated for a `.sql`
    /// file: one no file has as its module anymore is deleted.
    sql_generated: BTreeSet<PathBuf>,
    /// Problems with which `.sql` files there are.
    sql_problems: Vec<Diagnostic>,
}

/// A `.sql` file compiled alone, for a loader or the content mapper.
pub struct Compiled {
    /// The ES module a loader runs.
    pub module: String,
    /// The TypeScript the content mapper gives tsc, with its exports'
    /// names mapped to the file's (byte ranges: `(virtual start, original
    /// start, length)`).
    pub mapped: String,
    pub mappings: Vec<(usize, usize, usize)>,
    /// The file's errors: `(byte offset, length, message)`.
    pub errors: Vec<(usize, usize, String)>,
}

struct Database {
    config: DatabaseConfig,
    kind: Option<OutKind>,
    /// The generated files, as the resolver returns paths (canonical).
    resolved_outs: Vec<PathBuf>,
    /// The TS name each overridden PG type maps to.
    overrides: HashMap<QualifiedName, String>,
    /// The migrations the catalog was built from.
    migrations: Vec<(String, String)>,
    catalog: Result<PgCatalog, String>,
    /// Configured migration directories that don't exist, for the note.
    missing_dirs: Vec<PathBuf>,
    /// `(kind, text)` → its generated entries or error, kept across syncs.
    cache: HashMap<(Kind, String), Result<Generated, GenError>>,
}

/// Where a query is used: the file, its scan, the query's index in it.
type Uses<'a> = Vec<(&'a Path, &'a FileScan, usize)>;

pub const SOURCE_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts"];

/// Modules a re-export can be followed through.
const MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// How many modules a re-export chain is followed through.
const MAX_REEXPORT_DEPTH: usize = 16;

impl Project {
    /// A project for `config`. Unless `check`, a missing generated module is
    /// written empty first: sources import it, and an import that doesn't
    /// resolve can't be told apart from another module's.
    pub fn new(config: Config, check: bool) -> Result<Project, String> {
        Self::open(config, check, true)
    }

    /// A project that compiles `.sql` files alone: it scans no TypeScript
    /// source and writes nothing.
    pub fn sql_only(config: Config) -> Result<Project, String> {
        Self::open(config, true, false)
    }

    fn open(config: Config, check: bool, scan_sources: bool) -> Result<Project, String> {
        let mut dbs = Vec::new();
        for db in &config.databases {
            let kind = db.out.as_deref().map(out_kind).transpose()?;
            let mut resolved_outs = Vec::new();
            if let (Some(out), Some(kind)) = (&db.out, kind) {
                if !check {
                    ensure_exists(out, kind, &config.runtime)?;
                }
                resolved_outs.push(canonical(out));
                if let Some(d) = declaration_path(out) {
                    resolved_outs.push(canonical(&d));
                }
            }
            let mut names: HashMap<&str, &OverrideModule> = HashMap::new();
            for o in db.types.values() {
                if o.export == RUNTIME_NS {
                    return Err(format!(
                        "`types`: `{RUNTIME_NS}` names the runtime's types in the generated \
                         module, export the type under another name"
                    ));
                }
                if names
                    .insert(&o.export, &o.module)
                    .is_some_and(|m| *m != o.module)
                {
                    return Err(format!(
                        "`types`: two modules export a type named `{}`, which the generated \
                         module would import twice",
                        o.export
                    ));
                }
            }
            dbs.push(Database {
                config: db.clone(),
                kind,
                resolved_outs,
                overrides: db
                    .types
                    .iter()
                    .map(|(qn, o)| (qn.clone(), o.export.clone()))
                    .collect(),
                migrations: Vec::new(),
                catalog: Err("not built".to_owned()),
                missing_dirs: Vec::new(),
                cache: HashMap::new(),
            });
        }
        let resolver = Resolver::new(ResolveOptions {
            extensions: [
                ".ts", ".tsx", ".mts", ".cts", ".d.ts", ".js", ".jsx", ".mjs", ".cjs",
            ]
            .map(String::from)
            .to_vec(),
            // TypeScript's own rule: `./db.js` names `./db.ts`.
            extension_alias: vec![
                (
                    ".js".into(),
                    vec![".ts".into(), ".tsx".into(), ".js".into()],
                ),
                (".mjs".into(), vec![".mts".into(), ".mjs".into()]),
                (".cjs".into(), vec![".cts".into(), ".cjs".into()]),
            ],
            condition_names: ["types", "import", "require", "node", "default"]
                .map(String::from)
                .to_vec(),
            tsconfig: Some(TsconfigDiscovery::Auto),
            ..ResolveOptions::default()
        });
        let mut project = Project {
            config,
            dbs,
            resolver,
            files: BTreeMap::new(),
            modules: RefCell::new(HashMap::new()),
            scan_sources,
            sql_files: BTreeMap::new(),
            sql_outputs: BTreeMap::new(),
            sql_generated: BTreeSet::new(),
            sql_problems: Vec::new(),
        };
        for i in 0..project.dbs.len() {
            project.reload_migrations(i);
        }
        project.rescan_all();
        Ok(project)
    }

    /// Whether `path` is a source the project scans.
    pub fn is_source(&self, path: &Path) -> bool {
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SOURCE_EXTENSIONS.contains(&e));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        ext_ok
            && !name.ends_with(".d.ts")
            && !name.ends_with(".d.mts")
            && !name.ends_with(".d.cts")
            && self.config.include.iter().any(|inc| {
                path.strip_prefix(inc)
                    .is_ok_and(|rel| !rel.components().any(|c| is_ignored(c.as_os_str())))
            })
            && !self.is_generated(path)
    }

    /// Whether `path` is a generated file.
    pub fn is_generated(&self, path: &Path) -> bool {
        self.sql_outputs.contains_key(path)
            || self.dbs.iter().any(|db| {
                db.config.out.as_deref().is_some_and(|out| {
                    out == path || declaration_path(out).is_some_and(|d| d == path)
                })
            })
    }

    /// The database whose `.sql` files `path` is one of.
    pub fn sql_db(&self, path: &Path) -> Option<usize> {
        let root = &self.config.root;
        let rel = path.strip_prefix(root).ok()?;
        if rel.components().any(|c| is_ignored(c.as_os_str()))
            || self.dbs.iter().any(|db| {
                db.config
                    .migrations_dirs()
                    .iter()
                    .any(|d| path.starts_with(d))
            })
        {
            return None;
        }
        self.dbs.iter().position(|db| {
            db.config
                .sql
                .as_ref()
                .is_some_and(|sql| sql.matches(root, path))
        })
    }

    /// Whether `path` is (or, removed, was) a `.sql` file of the project.
    pub fn is_sql_source(&self, path: &Path) -> bool {
        self.sql_files.contains_key(path) || self.sql_db(path).is_some()
    }

    /// Read the `.sql` file at `path` again, or forget it if it is gone.
    pub fn rescan_sql(&mut self, path: &Path) {
        match (self.sql_db(path), std::fs::read_to_string(path)) {
            (Some(db), Ok(source)) => {
                self.sql_files
                    .insert(path.to_path_buf(), (db, parse_sql(&source)));
            }
            _ => {
                self.sql_files.remove(path);
            }
        }
        self.map_sql_outputs();
    }

    /// Find every `.sql` file, and the modules generated for files.
    fn collect_sql(&mut self) {
        self.sql_files.clear();
        let root = self.config.root.clone();
        let mut roots: Vec<PathBuf> = self
            .dbs
            .iter()
            .filter_map(|db| db.config.sql.as_ref())
            .flat_map(|sql| sql.roots(&root))
            .collect();
        roots.sort();
        roots.dedup();
        let mut found = Vec::new();
        for dir in &roots {
            collect_sources(dir, &mut found);
        }
        found.sort();
        found.dedup();
        for path in found {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "sql" {
                if let Some(db) = self.sql_db(&path)
                    && let Ok(source) = std::fs::read_to_string(&path)
                {
                    self.sql_files.insert(path, (db, parse_sql(&source)));
                }
            } else if MODULE_EXTENSIONS.contains(&ext) && is_sql_module(&path) {
                self.sql_generated.insert(path);
            }
        }
        self.map_sql_outputs();
    }

    /// Recompute which module each `.sql` file has.
    fn map_sql_outputs(&mut self) {
        self.sql_outputs.clear();
        self.sql_problems.clear();
        let root = &self.config.root;
        for (path, (db, _)) in &self.sql_files {
            let Some(sql) = &self.dbs[*db].config.sql else {
                continue;
            };
            for out in sql.outputs(root, path) {
                if out == *path {
                    continue;
                }
                if let Some(other) = self.sql_outputs.insert(out.clone(), path.clone()) {
                    self.sql_problems.push(Diagnostic {
                        file: path.clone(),
                        pos: None,
                        message: format!(
                            "its module would be {}, as {}'s: make `sql.out` tell them apart \
                             (`{{dir}}`, `{{name}}`)",
                            out.strip_prefix(root).unwrap_or(&out).display(),
                            other.strip_prefix(root).unwrap_or(&other).display()
                        ),
                    });
                }
            }
        }
    }

    /// Whether a source's re-exports were followed through `path`: a change
    /// to it can change what other sources' imports are.
    pub fn is_reexporter(&self, path: &Path) -> bool {
        self.modules.borrow().contains_key(&canonical(path))
    }

    /// The index of each database whose migrations `path` is in.
    pub fn migration_dbs(&self, path: &Path) -> Vec<usize> {
        (0..self.dbs.len())
            .filter(|&i| {
                self.dbs[i]
                    .config
                    .migrations_dirs()
                    .iter()
                    .any(|d| path.starts_with(d))
            })
            .collect()
    }

    /// Rescan every source and `.sql` file, forgetting how imports
    /// resolved.
    pub fn rescan_all(&mut self) {
        self.collect_sql();
        self.resolver.clear_cache();
        self.modules.borrow_mut().clear();
        self.files.clear();
        if !self.scan_sources || !self.dbs.iter().any(|db| db.config.out.is_some()) {
            return;
        }
        let mut sources = Vec::new();
        for inc in &self.config.include {
            collect_sources(inc, &mut sources);
        }
        sources.sort();
        sources.dedup();
        for path in sources {
            if self.is_source(&path) {
                self.rescan(&path);
            }
        }
    }

    /// Rescan `path`, or forget it if it is gone.
    pub fn rescan(&mut self, path: &Path) {
        let Ok(source) = std::fs::read_to_string(path) else {
            self.files.remove(path);
            return;
        };
        let dir = path.parent().unwrap_or(Path::new("."));
        let result = scan(path, &source, &mut |spec, name| {
            self.resolve_export(dir, spec, name, 0)
        });
        match result {
            Some(scan) => {
                self.files.insert(path.to_path_buf(), scan);
            }
            // Unparseable: keep the queries it had.
            None => {
                self.files.entry(path.to_path_buf()).or_default();
            }
        }
    }

    /// What the module `spec` (imported from `dir`) exports as `name`.
    fn resolve_export(&self, dir: &Path, spec: &str, name: &str, depth: usize) -> Option<Export> {
        let resolved = self.resolver.resolve(dir, spec).ok()?;
        self.export_of(resolved.path(), name, depth)
    }

    /// What the module at `path` exports as `name` (`*`: the module).
    fn export_of(&self, path: &Path, name: &str, depth: usize) -> Option<Export> {
        if let Some(db) = self
            .dbs
            .iter()
            .position(|d| d.resolved_outs.iter().any(|o| o == path))
        {
            return match name {
                "*" => Some(Export::Namespace { db }),
                n => Kind::of_export(n).map(|kind| Export::Item { db, kind }),
            };
        }
        let followable = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| MODULE_EXTENSIONS.contains(&e))
            && !path.components().any(|c| c.as_os_str() == "node_modules");
        if name == "*" || depth >= MAX_REEXPORT_DEPTH || !followable {
            return None;
        }
        let exports = {
            let mut modules = self.modules.borrow_mut();
            modules
                .entry(path.to_path_buf())
                .or_insert_with(|| {
                    let source = std::fs::read_to_string(path).ok()?;
                    module_exports(path, &source)
                })
                .clone()?
        };
        let dir = path.parent().unwrap_or(Path::new("."));
        for entry in &exports.entries {
            if let crate::scan::ExportEntry::Named {
                exported,
                spec,
                name: imported,
            } = entry
                && exported == name
            {
                return self.resolve_export(dir, spec, imported, depth + 1);
            }
        }
        if name == "default" {
            return None;
        }
        exports.entries.iter().find_map(|entry| match entry {
            crate::scan::ExportEntry::All { spec } => {
                self.resolve_export(dir, spec, name, depth + 1)
            }
            crate::scan::ExportEntry::Named { .. } => None,
        })
    }

    /// Rebuild database `i`'s catalog if its migrations changed.
    pub fn reload_migrations(&mut self, i: usize) {
        let root = self.config.root.clone();
        let db = &mut self.dbs[i];
        let dirs = db.config.migrations_dirs();
        db.missing_dirs = dirs.iter().filter(|d| !d.is_dir()).cloned().collect();
        let migrations = match collect_migrations(&dirs, &root) {
            Ok(m) => m,
            Err(e) => {
                db.catalog = Err(e);
                db.cache.clear();
                return;
            }
        };
        if migrations == db.migrations && db.catalog.is_ok() {
            return;
        }
        db.catalog =
            build_catalog(&migrations, db.config.runner.use_transaction).and_then(|mut catalog| {
                // Parameters bound from a `Date` are never infinite.
                let finite: Vec<QualifiedName> = crate::typemap::FINITE_PARAMETER_TYPES
                    .iter()
                    .map(|t| QualifiedName::new("pg_catalog", *t))
                    .filter(|q| !db.overrides.contains_key(q))
                    .collect();
                catalog.assume_finite_parameters(&finite)?;
                Ok(catalog)
            });
        db.migrations = migrations;
        db.cache.clear();
    }

    /// Regenerate every database's module and every `.sql` file's; write
    /// the changed files unless `check`.
    pub fn sync(&mut self, check: bool) -> SyncReport {
        let mut report = SyncReport::default();
        for (path, scan) in &self.files {
            for d in &scan.diagnostics {
                report.diagnostics.push(Diagnostic {
                    file: path.clone(),
                    pos: Some(d.pos),
                    message: d.message.clone(),
                });
            }
        }
        for (path, (_, file)) in &self.sql_files {
            for d in &file.diagnostics {
                report.diagnostics.push(Diagnostic {
                    file: path.clone(),
                    pos: Some(file.lines.position(d.offset)),
                    message: d.message.clone(),
                });
            }
        }
        report.diagnostics.extend(self.sql_problems.iter().cloned());
        for i in 0..self.dbs.len() {
            self.sync_db(i, check, &mut report);
        }
        self.remove_orphans(check, &mut report);
        report
            .diagnostics
            .sort_by(|a, b| (&a.file, a.pos).cmp(&(&b.file, b.pos)));
        report.diagnostics.dedup();
        report
    }

    fn sync_db(&mut self, i: usize, check: bool, report: &mut SyncReport) {
        let Project {
            config,
            dbs,
            files,
            sql_files,
            sql_outputs,
            sql_generated,
            ..
        } = self;
        // Each query, with where it is used.
        let mut queries: BTreeMap<(Kind, &str), Uses> = BTreeMap::new();
        for (path, scan) in files.iter() {
            for (n, q) in scan.queries.iter().enumerate() {
                if q.db == i {
                    queries
                        .entry((q.kind, &q.text))
                        .or_default()
                        .push((path, scan, n));
                }
            }
        }
        let sql_files: Vec<(&PathBuf, &SqlFile)> = sql_files
            .iter()
            .filter(|(_, (db, _))| *db == i)
            .map(|(path, (_, file))| (path, file))
            .collect();
        let sql_texts: HashSet<&str> = sql_files
            .iter()
            .flat_map(|(_, f)| f.queries.iter().map(|q| q.text.as_str()))
            .collect();
        let root = &config.root;
        let db = &mut dbs[i];
        let catalog = match &db.catalog {
            Ok(c) => c,
            Err(e) => {
                report.diagnostics.push(Diagnostic {
                    file: db.config.migrations_dir.clone(),
                    pos: None,
                    message: e.clone(),
                });
                return;
            }
        };
        let mapper = TypeMapper {
            overrides: &db.overrides,
            used: Default::default(),
        };
        // Drop what no source uses anymore.
        db.cache.retain(|(kind, text), _| {
            queries.contains_key(&(*kind, text.as_str()))
                || (*kind == Kind::Query && sql_texts.contains(text.as_str()))
        });
        let wanted = queries
            .keys()
            .copied()
            .chain(sql_texts.iter().map(|&t| (Kind::Query, t)));
        for (kind, text) in wanted {
            if let Entry::Vacant(e) = db.cache.entry((kind, text.to_owned())) {
                report.analyzed += 1;
                e.insert(generate(catalog, &mapper, kind, text));
            }
        }
        let note = missing_dirs_note(&db.missing_dirs, root);
        let with_note = |e: &GenError| {
            let mut message = e.message.trim_end().to_owned();
            if let Some(note) = &note {
                message.push_str("\n  note: ");
                message.push_str(note);
            }
            message
        };
        let mut entries: [Vec<(&str, &Result<Generated, GenError>)>; 2] = [Vec::new(), Vec::new()];
        for (&(kind, text), uses) in &queries {
            let result = &db.cache[&(kind, text.to_owned())];
            if let Err(e) = result {
                for (file, scan, n) in uses {
                    let q = &scan.queries[*n];
                    let pos = match e.offset {
                        Some(offset) => scan.lines.position(q.literal.source_offset(offset)),
                        None => q.pos,
                    };
                    report.diagnostics.push(Diagnostic {
                        file: file.to_path_buf(),
                        pos: Some(pos),
                        message: with_note(e),
                    });
                }
            }
            entries[(kind == Kind::CopyIn) as usize].push((text, result));
        }

        let mut checks = Vec::new();
        let mut types: Vec<_> = db.config.types.iter().collect();
        types.sort_by_key(|(qn, _)| *qn);
        for (qn, o) in types {
            match catalog.type_named(qn) {
                Some(ty) => checks.push(TypeCheck {
                    pg: qn.to_string(),
                    ts: o.export.clone(),
                    fits: mapper.map(&ty).codec.fits(),
                }),
                None => report.diagnostics.push(Diagnostic {
                    file: config.file.clone(),
                    pos: None,
                    message: format!(
                        "`types`: type \"{qn}\" does not exist (an unqualified name is in \
                         `public`; a built-in type is `pg_catalog.<name>`)"
                    ),
                }),
            }
        }

        // The `.sql` files' modules.
        for (path, file) in &sql_files {
            for q in &file.queries {
                if let Err(e) = &db.cache[&(Kind::Query, q.text.clone())] {
                    let at = e.offset.map_or(q.start, |o| q.start + o);
                    report.diagnostics.push(Diagnostic {
                        file: path.to_path_buf(),
                        pos: Some(file.lines.position(at)),
                        message: with_note(e),
                    });
                }
            }
            let Some(sql) = &db.config.sql else {
                continue;
            };
            let outputs = sql.outputs(root, path);
            // Another `.sql` file's module too: reported already.
            if outputs.iter().any(|o| sql_outputs.get(o) != Some(*path)) {
                continue;
            }
            let Some(first) = outputs.first() else {
                continue;
            };
            let dir = first.parent().unwrap_or(Path::new("."));
            let imports = override_imports(&db.config, dir);
            let queries = sql_queries(file, &db.cache, &checks);
            let module = SqlModule {
                runtime: &config.runtime,
                source: source_from(first, path),
                imports: &imports,
                queries: &queries,
                type_names: &sql.type_names,
            };
            let contents = match sql.emit {
                Emit::Ts => {
                    let kind = out_kind(first).expect("validated with the config");
                    module.files(first, kind)
                }
                Emit::Dts => vec![(first.clone(), module.declaration_file())],
                Emit::None => Vec::new(),
            };
            for (out, content) in contents {
                let current = std::fs::read_to_string(&out).ok();
                if current.as_deref() == Some(content.as_str()) {
                    sql_generated.insert(out);
                    continue;
                }
                if let Some(current) = &current
                    && !overwritable(current)
                {
                    report.diagnostics.push(Diagnostic {
                        file: path.to_path_buf(),
                        pos: None,
                        message: format!(
                            "its module would be {}, which is not a module typedpg generated: \
                             move that file, or set `sql.out`",
                            out.strip_prefix(root).unwrap_or(&out).display()
                        ),
                    });
                    continue;
                }
                if !check && let Err(e) = write_file(&out, &content) {
                    report.diagnostics.push(Diagnostic {
                        file: out.clone(),
                        pos: None,
                        message: format!("failed to write: {e}"),
                    });
                }
                sql_generated.insert(out.clone());
                report.changed.push(out);
            }
        }

        let Some(out) = &db.config.out else {
            return;
        };
        let embedded = if db.config.embed_migrations {
            match typedpg::migrate::MigrationSource::from_dir(&db.config.migrations_dir) {
                Ok(source) => Some(
                    source
                        .migrations()
                        .iter()
                        .map(|m| (m.name.clone(), m.sql.clone(), m.down_sql.clone()))
                        .collect::<Vec<_>>(),
                ),
                Err(e) => {
                    report.diagnostics.push(Diagnostic {
                        file: db.config.migrations_dir.clone(),
                        pos: None,
                        message: e.to_string(),
                    });
                    return;
                }
            }
        } else {
            None
        };
        let imports = override_imports(&db.config, out.parent().unwrap_or(Path::new(".")));
        let module = Module {
            runtime: &config.runtime,
            imports: &imports,
            checks: &checks,
            queries: &entries[0],
            copies: &entries[1],
            migrations: embedded.as_deref().map(|migrations| Embedded {
                migrations,
                runner: &db.config.runner,
            }),
        };
        let kind = db.kind.expect("a database with `out` has its kind");
        for (path, content) in module.files(out, kind) {
            let current = std::fs::read_to_string(&path).unwrap_or_default();
            if current != content {
                if !check && let Err(e) = write_file(&path, &content) {
                    report.diagnostics.push(Diagnostic {
                        file: path.clone(),
                        pos: None,
                        message: format!("failed to write: {e}"),
                    });
                }
                report.changed.push(path);
            }
        }
    }

    /// Delete the modules generated for `.sql` files that no file has as
    /// its module anymore (a file removed, or `sql` changed).
    fn remove_orphans(&mut self, check: bool, report: &mut SyncReport) {
        let orphans: Vec<PathBuf> = self
            .sql_generated
            .iter()
            .filter(|p| !self.sql_outputs.contains_key(*p))
            .cloned()
            .collect();
        for path in orphans {
            self.sql_generated.remove(&path);
            if self.is_generated(&path) || !is_sql_module(&path) {
                continue;
            }
            if !check && let Err(e) = std::fs::remove_file(&path) {
                report.diagnostics.push(Diagnostic {
                    file: path.clone(),
                    pos: None,
                    message: format!("failed to delete: {e}"),
                });
            }
            report.removed.push(path);
        }
    }

    /// Compile the `.sql` file at `path`, whose text is `content`, alone:
    /// for a loader or the content mapper.
    pub fn compile_sql(&mut self, path: &Path, content: &str) -> Result<Compiled, String> {
        let i = self.sql_db(path).ok_or_else(|| {
            format!(
                "{} is not one of the .sql files of {}: add it to `sql.include`",
                path.display(),
                self.config.file.display()
            )
        })?;
        let file = parse_sql(content);
        let db = &mut self.dbs[i];
        let catalog = db.catalog.as_ref().map_err(Clone::clone)?;
        let mapper = TypeMapper {
            overrides: &db.overrides,
            used: Default::default(),
        };
        for q in &file.queries {
            if let Entry::Vacant(e) = db.cache.entry((Kind::Query, q.text.clone())) {
                e.insert(generate(catalog, &mapper, Kind::Query, &q.text));
            }
        }
        let checks: Vec<TypeCheck> = db
            .config
            .types
            .iter()
            .filter_map(|(qn, o)| {
                Some(TypeCheck {
                    pg: qn.to_string(),
                    ts: o.export.clone(),
                    fits: mapper.map(&catalog.type_named(qn)?).codec.fits(),
                })
            })
            .collect();
        let mut errors: Vec<(usize, usize, String)> = file
            .diagnostics
            .iter()
            .map(|d| (d.offset, token_len(content, d.offset), d.message.clone()))
            .collect();
        for q in &file.queries {
            if let Err(e) = &db.cache[&(Kind::Query, q.text.clone())] {
                let at = e.offset.map_or(q.start, |o| q.start + o);
                errors.push((at, token_len(content, at), e.message.trim_end().to_owned()));
            }
        }
        let dir = path.parent().unwrap_or(Path::new("."));
        let imports = override_imports(&db.config, dir);
        let queries = sql_queries(&file, &db.cache, &checks);
        let type_names = db
            .config
            .sql
            .as_ref()
            .map(|s| s.type_names.clone())
            .expect("a database with .sql files has `sql`");
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let module = SqlModule {
            runtime: &self.config.runtime,
            source: name,
            imports: &imports,
            queries: &queries,
            type_names: &type_names,
        };
        let Mapped { text, exports } = module.mapped();
        let mappings = exports
            .iter()
            .map(|&(at, q)| {
                let span = &file.queries[q].name_span;
                (at, span.start, span.len())
            })
            .collect();
        Ok(Compiled {
            module: module.loader_module(),
            mapped: text,
            mappings,
            errors,
        })
    }

    /// The `.sql` files, with their databases.
    pub fn sql_paths(&self) -> impl Iterator<Item = &Path> {
        self.sql_files.keys().map(PathBuf::as_path)
    }

    /// Every file database `i`'s catalog is built from.
    pub fn migration_files(&self, i: usize) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for dir in self.dbs[i].config.migrations_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "sql") {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }
}

/// The queries of `file` and what was generated for each, with the checks
/// of the overrides each uses.
fn sql_queries<'a>(
    file: &'a SqlFile,
    cache: &'a HashMap<(Kind, String), Result<Generated, GenError>>,
    checks: &'a [TypeCheck],
) -> Vec<SqlQuery<'a>> {
    file.queries
        .iter()
        .map(|q| {
            let result = &cache[&(Kind::Query, q.text.clone())];
            let used = result
                .as_ref()
                .map(|g| g.overrides.as_slice())
                .unwrap_or(&[]);
            SqlQuery {
                name: &q.name,
                text: &q.text,
                result,
                checks: checks.iter().filter(|c| used.contains(&c.ts)).collect(),
            }
        })
        .collect()
}

/// The length of the word at byte `at` of `text` (at least a character),
/// for an error's span.
fn token_len(text: &str, at: usize) -> usize {
    let rest = text.get(at..).unwrap_or("");
    let word = rest
        .char_indices()
        .find(|&(_, c)| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .map_or(rest.len(), |(k, _)| k);
    if word > 0 {
        word
    } else {
        rest.chars().next().map_or(0, char::len_utf8)
    }
}

/// Whether the file at `path` is a module generated for a `.sql` file.
fn is_sql_module(path: &Path) -> bool {
    first_line(path).is_some_and(|l| generated_source(&l).is_some())
}

fn first_line(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut buf = [0u8; 512];
    let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    Some(text.lines().next().unwrap_or("").to_owned())
}

/// Whether a file with `content` can be overwritten by a `.sql` file's
/// module: one typedpg generated, or pgtyped (which generated the module
/// of a `.sql` file at the same place).
fn overwritable(content: &str) -> bool {
    let first = content.lines().next().unwrap_or("");
    generated_source(first).is_some()
        || first.starts_with("/** Types generated for queries found in \"")
}

/// Rust's note for a migrations directory that isn't there: every table is
/// then missing, and a typo in the path would look like that.
fn missing_dirs_note(missing: &[PathBuf], root: &Path) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    let dirs: Vec<String> = missing
        .iter()
        .map(|d| format!("'{}'", d.strip_prefix(root).unwrap_or(d).display()))
        .collect();
    Some(format!(
        "no migrations were loaded from {}: the directory does not exist (see `migrations` in \
         typedpg.config.json)",
        dirs.join(", ")
    ))
}

/// The `import type` lines a database's overrides need, relative to the
/// directory of a module.
fn override_imports(db: &DatabaseConfig, out_dir: &Path) -> Vec<(String, String)> {
    let mut imports: Vec<(String, String)> = db
        .types
        .values()
        .map(|o| {
            let module = match &o.module {
                OverrideModule::Package(p) => p.clone(),
                OverrideModule::Path(p) => relative_specifier(out_dir, p),
            };
            (module, o.export.clone())
        })
        .collect();
    imports.sort();
    imports.dedup();
    imports
}

/// The `./`-prefixed specifier of `target` from `from_dir`.
pub fn relative_specifier(from_dir: &Path, target: &Path) -> String {
    let from: Vec<_> = normalize(from_dir)
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let to: Vec<_> = normalize(target)
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_owned(); from.len() - common];
    parts.extend(
        to[common..]
            .iter()
            .map(|c| c.to_string_lossy().into_owned()),
    );
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

/// `path` without `.` components and with `..` applied.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// `path` as the resolver reports it: canonical when it exists.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| normalize(path))
}

fn write_file(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content)
}

/// Write an empty module at `out` if there is none.
fn ensure_exists(out: &Path, kind: OutKind, runtime: &str) -> Result<(), String> {
    let module = Module {
        runtime,
        imports: &[],
        checks: &[],
        queries: &[],
        copies: &[],
        migrations: None,
    };
    for (path, content) in module.files(out, kind) {
        if !path.exists() {
            write_file(&path, &content).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(())
}

/// Whether a directory (or file) name is skipped: `node_modules` and hidden
/// ones.
pub fn is_ignored(name: &std::ffi::OsStr) -> bool {
    let s = name.to_string_lossy();
    s == "node_modules" || (s.starts_with('.') && s != "." && s != "..")
}

fn collect_sources(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        if is_ignored(&entry.file_name()) {
            continue;
        }
        let p = entry.path();
        if p.is_dir() {
            collect_sources(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// `(path, SQL)` of each migration, sorted by file name (whatever the
/// directory), as the Rust side applies them; the path relative to `root`,
/// as diagnostics show it. A missing directory has none.
fn collect_migrations(dirs: &[PathBuf], root: &Path) -> Result<Vec<(String, String)>, String> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.to_string_lossy().into_owned();
            if name.ends_with(".sql") && !name.ends_with(".down.sql") && seen.insert(path.clone()) {
                let sql = std::fs::read_to_string(&path)
                    .map_err(|e| format!("failed to read migration '{name}': {e}"))?;
                let shown = path.strip_prefix(root).unwrap_or(&path);
                files.push((entry.file_name(), shown.display().to_string(), sql));
            }
        }
    }
    // The migrations are applied in the order the runner applies them: by
    // their numeric prefix, not by their name.
    files.sort_by(|a, b| {
        typedpg_core::migrations::compare_migration_names(
            &a.0.to_string_lossy(),
            &b.0.to_string_lossy(),
        )
    });
    Ok(files
        .into_iter()
        .map(|(_, name, sql)| (name, sql))
        .collect())
}

fn build_catalog(
    migrations: &[(String, String)],
    use_transaction: bool,
) -> Result<PgCatalog, String> {
    let mut catalog =
        PgCatalog::new().map_err(|e| format!("failed to load the PG catalog seed: {e}"))?;
    catalog.set_migrations_use_transaction(use_transaction);
    for (name, sql) in migrations {
        // The error is rendered with `--> file:line:col`.
        catalog
            .apply_migration(name, sql)
            .map_err(|e| e.to_string())?;
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_specifiers() {
        assert_eq!(
            relative_specifier(Path::new("/p/src/db"), Path::new("/p/./src/domains")),
            "../domains"
        );
        assert_eq!(
            relative_specifier(Path::new("/p/src"), Path::new("/p/src/types/x.js")),
            "./types/x.js"
        );
    }
}
