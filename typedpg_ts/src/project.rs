//! The state a run keeps: each source's queries, each database's catalog
//! and analyzed queries. `gen` builds it once; `--watch` keeps it and
//! updates only what changed — a source rescans just that file, a query
//! already analyzed is not analyzed again, and a generated module is
//! rewritten only when its content changes.

use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};
use typedpg_analyzer::PgCatalog;
use typedpg_core::QualifiedName;

use crate::config::{Config, DatabaseConfig, OutKind, OverrideModule, declaration_path, out_kind};
use crate::emit::{Embedded, Module, TypeCheck};
use crate::generate::{GenError, Generated, generate};
use crate::scan::{Export, FileScan, Kind, ModuleExports, Position, module_exports, scan};
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
}

struct Database {
    config: DatabaseConfig,
    kind: OutKind,
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
        let mut dbs = Vec::new();
        for db in &config.databases {
            let kind = out_kind(&db.out)?;
            if !check {
                ensure_exists(db, kind, &config.runtime)?;
            }
            let mut resolved_outs = vec![canonical(&db.out)];
            if let Some(d) = declaration_path(&db.out) {
                resolved_outs.push(canonical(&d));
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
        self.dbs.iter().any(|db| {
            db.config.out == path || declaration_path(&db.config.out).is_some_and(|d| d == path)
        })
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

    /// Rescan every source, forgetting how imports resolved.
    pub fn rescan_all(&mut self) {
        self.resolver.clear_cache();
        self.modules.borrow_mut().clear();
        self.files.clear();
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
        db.catalog = build_catalog(&migrations, db.config.runner.use_transaction);
        db.migrations = migrations;
        db.cache.clear();
    }

    /// Regenerate every database's module; write the changed files unless
    /// `check`.
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
        for i in 0..self.dbs.len() {
            self.sync_db(i, check, &mut report);
        }
        report
            .diagnostics
            .sort_by(|a, b| (&a.file, a.pos).cmp(&(&b.file, b.pos)));
        report.diagnostics.dedup();
        report
    }

    fn sync_db(&mut self, i: usize, check: bool, report: &mut SyncReport) {
        // Each query, with where it is used.
        let mut queries: BTreeMap<(Kind, &str), Uses> = BTreeMap::new();
        for (path, scan) in &self.files {
            for (n, q) in scan.queries.iter().enumerate() {
                if q.db == i {
                    queries
                        .entry((q.kind, &q.text))
                        .or_default()
                        .push((path, scan, n));
                }
            }
        }
        let root = &self.config.root;
        let db = &mut self.dbs[i];
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
            int8: db.config.int8,
        };
        // Drop what no source uses anymore.
        db.cache
            .retain(|(kind, text), _| queries.contains_key(&(*kind, text.as_str())));
        for &(kind, text) in queries.keys() {
            if let Entry::Vacant(e) = db.cache.entry((kind, text.to_owned())) {
                report.analyzed += 1;
                e.insert(generate(catalog, &mapper, kind, text));
            }
        }
        let note = missing_dirs_note(&db.missing_dirs, root);
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
                    let mut message = e.message.trim_end().to_owned();
                    if let Some(note) = &note {
                        message.push_str("\n  note: ");
                        message.push_str(note);
                    }
                    report.diagnostics.push(Diagnostic {
                        file: file.to_path_buf(),
                        pos: Some(pos),
                        message,
                    });
                }
            }
            entries[(kind == Kind::CopyIn) as usize].push((text, result));
        }

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
        let imports = override_imports(&db.config);
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
                    file: self.config.file.clone(),
                    pos: None,
                    message: format!(
                        "`types`: type \"{qn}\" does not exist (an unqualified name is in \
                         `public`; a built-in type is `pg_catalog.<name>`)"
                    ),
                }),
            }
        }
        let module = Module {
            runtime: &self.config.runtime,
            imports: &imports,
            checks: &checks,
            queries: &entries[0],
            copies: &entries[1],
            migrations: embedded.as_deref().map(|migrations| Embedded {
                migrations,
                runner: &db.config.runner,
            }),
        };
        for (path, content) in module.files(&db.config.out, db.kind) {
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

/// The `import type` lines a database's overrides need, relative to its
/// generated module.
fn override_imports(db: &DatabaseConfig) -> Vec<(String, String)> {
    let out_dir = db.out.parent().unwrap_or(Path::new("."));
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
fn relative_specifier(from_dir: &Path, target: &Path) -> String {
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

/// Write an empty module for `db` if there is none.
fn ensure_exists(db: &DatabaseConfig, kind: OutKind, runtime: &str) -> Result<(), String> {
    let module = Module {
        runtime,
        imports: &[],
        checks: &[],
        queries: &[],
        copies: &[],
        migrations: None,
    };
    for (path, content) in module.files(&db.out, kind) {
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
    files.sort_by(|a, b| a.0.cmp(&b.0));
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
