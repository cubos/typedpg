//! `typedpg gen --watch`: keep the [`Project`] and update it as files
//! change — a source is rescanned alone, a migration rebuilds its
//! database's catalog, the config file reloads everything.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{EventKind, RecursiveMode, Watcher};

use crate::config::Config;
use crate::project::{Project, SyncReport};

/// How long to wait for more events after one arrives, so that an editor's
/// save (often several events) or a `git checkout` is handled in one go.
const DEBOUNCE: Duration = Duration::from_millis(50);

pub fn run(config_path: &Path) -> Result<(), String> {
    let config_path = std::path::absolute(config_path).map_err(|e| e.to_string())?;
    let mut project = Project::new(Config::load(&config_path)?, false)?;
    let start = Instant::now();
    let report = project.sync(false);
    print_report(&project, &report, start, "initial generation");

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx).map_err(|e| e.to_string())?;
    for dir in watch_roots(&project) {
        watch_tree(&mut watcher, &dir);
    }
    eprintln!("watching for changes…");

    loop {
        let Ok(first) = rx.recv() else {
            return Ok(());
        };
        let mut events = vec![first];
        while let Ok(ev) = rx.recv_timeout(DEBOUNCE) {
            events.push(ev);
        }
        let mut paths = BTreeSet::new();
        // Paths created, removed or renamed: an import elsewhere may now
        // resolve differently.
        let mut structural = BTreeSet::new();
        for ev in events.into_iter().flatten() {
            if matches!(ev.kind, EventKind::Access(_)) {
                continue;
            }
            if matches!(
                ev.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
            ) {
                structural.extend(ev.paths.iter().cloned());
            }
            // A new directory: watch it too (the recursive watch of its
            // parent may not exist, see `watch_tree`).
            if matches!(ev.kind, EventKind::Create(_)) {
                for p in &ev.paths {
                    if p.is_dir() && !is_ignored(p) {
                        watch_tree(&mut watcher, p);
                    }
                }
            }
            paths.extend(ev.paths);
        }

        let start = Instant::now();
        let mut what = Vec::new();
        if paths.contains(&config_path) {
            match Config::load(&config_path).and_then(|c| Project::new(c, false)) {
                Ok(p) => {
                    project = p;
                    what.push("config reloaded".to_owned());
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    continue;
                }
            }
        } else {
            let mut rescanned = 0;
            let mut resolution_changed = false;
            let mut migrations = BTreeSet::new();
            for p in &paths {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "package.json"
                    || (name.starts_with("tsconfig") && name.ends_with(".json"))
                {
                    resolution_changed = true;
                } else if project.is_generated(p) {
                    // Our own writes.
                } else if project.is_reexporter(p)
                    || (structural.contains(p) && project.is_source(p))
                {
                    resolution_changed = true;
                } else if project.is_source(p) {
                    project.rescan(p);
                    rescanned += 1;
                } else {
                    migrations.extend(project.migration_dbs(p));
                }
            }
            if resolution_changed {
                project.rescan_all();
                what.push("module resolution may have changed, every source rescanned".to_owned());
            } else if rescanned > 0 {
                what.push(format!(
                    "{rescanned} source{} rescanned",
                    if rescanned == 1 { "" } else { "s" }
                ));
            }
            for &db in &migrations {
                project.reload_migrations(db);
            }
            if !migrations.is_empty() {
                what.push("migrations reloaded".to_owned());
            }
        }
        if what.is_empty() {
            continue;
        }
        let report = project.sync(false);
        print_report(&project, &report, start, &what.join(", "));
    }
}

/// Print a sync's diagnostics and a one-line summary.
pub fn print_report(project: &Project, report: &SyncReport, start: Instant, what: &str) {
    let root = &project.config.root;
    for d in &report.diagnostics {
        eprintln!("{}", d.render(root));
    }
    let written: Vec<String> = report
        .changed
        .iter()
        .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string())
        .collect();
    eprintln!(
        "[typedpg] {what}: {} quer{} analyzed, {}, {} error{} ({} ms)",
        report.analyzed,
        if report.analyzed == 1 { "y" } else { "ies" },
        if written.is_empty() {
            "output unchanged".to_owned()
        } else {
            format!("wrote {}", written.join(", "))
        },
        report.diagnostics.len(),
        if report.diagnostics.len() == 1 {
            ""
        } else {
            "s"
        },
        start.elapsed().as_millis()
    );
}

/// The directories to watch: the project's root, and what is configured
/// outside it.
fn watch_roots(project: &Project) -> Vec<PathBuf> {
    let root = &project.config.root;
    let mut dirs = vec![root.clone()];
    let outside: Vec<PathBuf> = project
        .config
        .include
        .iter()
        .cloned()
        .chain(
            project
                .config
                .databases
                .iter()
                .flat_map(|d| d.migrations_dirs()),
        )
        .filter(|d| !d.starts_with(root) && d.is_dir())
        .collect();
    dirs.extend(outside);
    dirs
}

/// Watch `dir` and, recursively, each subdirectory that isn't ignored. Not a
/// recursive watch of `dir`: on Linux that is one inotify watch per
/// directory, `node_modules` included.
fn watch_tree(watcher: &mut impl Watcher, dir: &Path) {
    let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() && !is_ignored(&p) {
            watch_tree(watcher, &p);
        }
    }
}

fn is_ignored(path: &Path) -> bool {
    path.file_name().is_some_and(crate::project::is_ignored)
}
