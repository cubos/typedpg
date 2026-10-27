//! `typedpg serve`: one long-lived process for the two things that would
//! otherwise each spawn a binary per file.
//!
//! TypeScript 7.1's content mapper (`contentMappers` in tsconfig.json,
//! `tsc --runExternalCode`) runs it over JSON-RPC on stdin / stdout with
//! LSP's `Content-Length` framing, and sends every request: `initialize`,
//! then `openProject` per project, `transform` per file, `closeProject`. A
//! file is transformed into the module `emit: "ts"` would write for it —
//! the same types, `createBrokenQuery` for a query with an error — each
//! export's name mapped to its `@name`, and the file's errors as
//! diagnostics at their place in it.
//!
//! The `.sql` loaders and bundler plugins send `typedpg/compile` instead:
//! `{ "config": <typedpg.config.json>, "file": <the .sql file> }`, with no
//! handshake before it. The project's schema stays in memory between
//! requests — rebuilt only when its identity changes, the config and
//! migrations' hash — and the file's module is written to the cache
//! `typedpg build` fills, with the same key, so a later process or a cold
//! loader reuses it. The result carries the module, the files to watch and
//! the file's errors as `typedpg build --json` reports them.
//!
//! The project's typedpg config is the mapper's `"config"` option (a path
//! relative to tsconfig.json), else the nearest `typedpg.config.json` above
//! tsconfig.json. Its identity — what TypeScript keys its incremental state
//! on — is the version and the config and migrations' hash, and the files
//! it watches are the config file and the migrations.
//!
//! `--idle-timeout` makes the process exit (status 0) after that many
//! seconds without a request: the loaders run a child per binary path and
//! let it go once the project has been idle for a while. TypeScript runs it
//! without one. End of stdin — the parent is gone — exits it either way.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use serde_json::{Value, json};

use crate::cache;
use crate::config::{CONFIG_FILE, Config};
use crate::project::Project;
use crate::scan::LineIndex;

/// Diagnostic codes, shown as `typedpg<code>`.
const CODE_QUERY: u32 = 1;
const CODE_PROJECT: u32 = 2;

struct Opened {
    /// The project, or why there is none (reported on each file).
    project: Result<Project, String>,
}

/// A project kept for the loaders, by its config file.
struct CompileProject {
    /// The config and migrations' hash it was opened with: a different one
    /// means the schema, and so every compiled module, can differ.
    identity: String,
    /// The project, or why it could not open (reported on each file).
    project: Result<Project, String>,
}

#[derive(Default)]
struct State {
    /// TypeScript's projects, by handle (`openProject`).
    projects: HashMap<String, Opened>,
    /// The loaders' projects, by config file.
    compile: HashMap<PathBuf, CompileProject>,
}

/// What the stdin reader sends the loop.
enum Event {
    /// A request is about to be read: the idle timeout starts over, however
    /// long its bytes take to arrive.
    Activity,
    Message(Vec<u8>),
    /// End of stdin: the parent closed it (or is gone).
    End,
    /// Reading stdin failed.
    Failed(String),
}

/// Serve, exiting after `idle_timeout` seconds without a request when set.
pub fn run(idle_timeout: Option<f64>) -> Result<(), String> {
    let idle = match idle_timeout {
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            Some(Duration::from_secs_f64(seconds))
        }
        Some(_) => {
            return Err("`--idle-timeout` takes a positive number of seconds".to_owned());
        }
        None => None,
    };
    serve(idle)
}

fn serve(idle: Option<Duration>) -> Result<(), String> {
    // A reader thread, so that the loop can wait for a request with a
    // timeout: a blocking `read_line` could not be interrupted when idle.
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut input = BufReader::new(stdin.lock());
        loop {
            if sender.send(Event::Activity).is_err() {
                return;
            }
            match read_message(&mut input) {
                Ok(Some(message)) => {
                    if sender.send(Event::Message(message)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = sender.send(Event::End);
                    return;
                }
                Err(e) => {
                    let _ = sender.send(Event::Failed(e.to_string()));
                    return;
                }
            }
        }
    });

    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut state = State::default();
    loop {
        let event = match idle {
            None => receiver
                .recv()
                .map_err(|_| "the stdin reader stopped".to_owned())?,
            Some(timeout) => match receiver.recv_timeout(timeout) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return Ok(()),
            },
        };
        match event {
            Event::Activity => {}
            Event::End => return Ok(()),
            Event::Failed(message) => return Err(message),
            Event::Message(message) => {
                let Ok(message) = serde_json::from_slice::<Value>(&message) else {
                    continue;
                };
                // Notifications: none is defined; nothing to answer.
                let Some(id) = message.get("id").cloned() else {
                    continue;
                };
                let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let response = match handle(method, &params, &mut state) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err((code, message)) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": code, "message": message },
                    }),
                };
                write_message(&mut output, &response).map_err(|e| e.to_string())?;
            }
        }
    }
}

type RpcError = (i64, String);

fn handle(method: &str, params: &Value, state: &mut State) -> Result<Value, RpcError> {
    let str_param = |name: &str| -> Result<&str, RpcError> {
        params
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| (-32602, format!("missing `{name}`")))
    };
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": 1,
            // Byte offsets: what the analyzer and the file reader give.
            "positionEncoding": "utf-8",
            "diagnosticSource": "typedpg",
        })),
        "openProject" => {
            let handle = str_param("projectHandle")?.to_owned();
            let tsconfig = str_param("configFileName")?;
            let option = params
                .get("options")
                .and_then(|o| o.get("config"))
                .and_then(Value::as_str);
            let (opened, result) = open_project(tsconfig, option);
            state.projects.insert(handle, opened);
            Ok(result)
        }
        "closeProject" => {
            state.projects.remove(str_param("projectHandle")?);
            Ok(Value::Null)
        }
        "transform" => {
            let handle = str_param("projectHandle")?;
            let file = PathBuf::from(str_param("fileName")?);
            let content = str_param("content")?;
            let opened = state
                .projects
                .get_mut(handle)
                .ok_or_else(|| (-32602, format!("unknown project `{handle}`")))?;
            Ok(transform(opened, &file, content))
        }
        // The loaders' method: usable without `initialize` / `openProject`.
        "typedpg/compile" => compile(
            state,
            &PathBuf::from(str_param("config")?),
            &PathBuf::from(str_param("file")?),
        ),
        _ => Err((-32601, format!("unknown method `{method}`"))),
    }
}

fn open_project(tsconfig: &str, option: Option<&str>) -> (Opened, Value) {
    let dir = if tsconfig.is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        Path::new(tsconfig)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    };
    let config_path = match option {
        Some(p) => Some(dir.join(p)),
        None => dir
            .ancestors()
            .map(|d| d.join(CONFIG_FILE))
            .find(|p| p.is_file()),
    };
    let Some(config_path) = config_path else {
        let message = format!(
            "no {CONFIG_FILE} in {} or a directory above it: set the content mapper's \
             `\"config\"` option",
            dir.display()
        );
        let result = json!({
            "configIdentity": format!("{}:none", env!("CARGO_PKG_VERSION")),
            "watchedFiles": [],
        });
        return (
            Opened {
                project: Err(message),
            },
            result,
        );
    };
    let config = Config::load(&config_path);
    let identity = config
        .as_ref()
        .ok()
        .and_then(|c| cache::schema_identity(c).ok())
        .unwrap_or_else(|| "unreadable".to_owned());
    let mut watched = vec![config_path.to_string_lossy().into_owned()];
    let project = config.and_then(Project::sql_only);
    if let Ok(p) = &project {
        for i in 0..p.config.databases.len() {
            watched.extend(
                p.migration_files(i)
                    .iter()
                    .map(|f| f.to_string_lossy().into_owned()),
            );
        }
    }
    let result = json!({
        "configIdentity": format!("{}:{identity}", env!("CARGO_PKG_VERSION")),
        "watchedFiles": watched,
    });
    (Opened { project }, result)
}

fn transform(opened: &mut Opened, file: &Path, content: &str) -> Value {
    let compiled = match &mut opened.project {
        Ok(project) => project.compile_sql(file, content),
        Err(e) => Err(e.clone()),
    };
    match compiled {
        Ok(c) => json!({
            "text": c.mapped,
            "extension": ".ts",
            "mappings": c
                .mappings
                .iter()
                .map(|&(virtual_start, original_start, len)| {
                    // Verbatim: the export is named as the query.
                    json!([virtual_start, len, original_start, len, 0])
                })
                .collect::<Vec<_>>(),
            "diagnostics": c
                .errors
                .iter()
                .map(|(start, length, message)| json!({
                    "messageText": without_snippet(message),
                    "start": start,
                    "length": length,
                    "code": CODE_QUERY,
                }))
                .collect::<Vec<_>>(),
        }),
        Err(message) => json!({
            "text": "export {};\n",
            "extension": ".ts",
            "mappings": [],
            "diagnostics": [{
                "messageText": message,
                "start": 0,
                "length": 0,
                "code": CODE_PROJECT,
            }],
        }),
    }
}

/// Compile one `.sql` file for a loader, reusing the project's schema until
/// its identity changes, and fill the cache entry a later process reads.
fn compile(state: &mut State, config_path: &Path, file: &Path) -> Result<Value, RpcError> {
    let config = Config::load(config_path).map_err(|e| (-32603, e))?;
    let identity = cache::schema_identity(&config).map_err(|e| (-32603, e))?;
    let reload = state
        .compile
        .get(&config.file)
        .is_none_or(|p| p.identity != identity);
    if reload {
        state.compile.insert(
            config.file.clone(),
            CompileProject {
                identity: identity.clone(),
                project: Project::sql_only(config.clone()),
            },
        );
    }
    let opened = state
        .compile
        .get_mut(&config.file)
        .expect("the project was just opened");
    let project = opened.project.as_mut().map_err(|e| (-32603, e.clone()))?;
    let content = std::fs::read_to_string(file)
        .map_err(|e| (-32603, format!("failed to read {}: {e}", file.display())))?;
    let dependencies = compile_dependencies(&config);
    let compiled = match project.compile_sql(file, &content) {
        Ok(compiled) => compiled,
        // Not one of the project's `.sql` files: `typedpg build` does not
        // compile it either, and the loader says to add it to `sql.include`.
        Err(_) => {
            return Ok(json!({
                "code": null,
                "dependencies": dependencies,
                "diagnostics": [],
            }));
        }
    };
    let diagnostics = if compiled.errors.is_empty() {
        // The same key and recipe as `typedpg build`: a later process, or a
        // cold loader, finds the module without this one.
        let dir = cache::dir(&config);
        let key = cache::key(&config, &identity, file, &content);
        cache::write(&dir, &key, &compiled.module).map_err(|e| {
            (
                -32603,
                format!("failed to write the cache in {}: {e}", dir.display()),
            )
        })?;
        Vec::new()
    } else {
        // As `typedpg build --json` reports them: the file relative to the
        // project, the error at its line and column (both 1-based).
        let shown = file
            .strip_prefix(&config.root)
            .unwrap_or(file)
            .display()
            .to_string();
        let lines = LineIndex::new(&content);
        compiled
            .errors
            .iter()
            .map(|(offset, _, message)| {
                let pos = lines.position(*offset);
                json!({
                    "file": shown,
                    "line": pos.line,
                    "col": pos.col,
                    "message": message,
                })
            })
            .collect()
    };
    Ok(json!({
        "code": compiled.module,
        "dependencies": dependencies,
        "diagnostics": diagnostics,
    }))
}

/// The config file and every migration of every database: what the loaders
/// watch (sqlfiles.ts's `project.files`). `.down.sql` files change no schema.
fn compile_dependencies(config: &Config) -> Vec<String> {
    let mut files = vec![config.file.to_string_lossy().into_owned()];
    for db in &config.databases {
        for dir in db.migrations_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(".sql") && !name.ends_with(".down.sql") {
                    files.push(entry.path().to_string_lossy().into_owned());
                }
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// An error's message without the snippet the CLI renders under its
/// first line (its line numbers are the query's, and the editor shows the
/// place): the first line and the `help:` / `note:` ones.
fn without_snippet(message: &str) -> String {
    let mut lines = message.lines();
    let mut out = lines.next().unwrap_or_default().to_owned();
    for line in lines {
        let line = line.trim_start();
        if line.starts_with("help:") || line.starts_with("note:") {
            out.push('\n');
            out.push_str(line);
        }
    }
    out
}

/// The next message's body; `None` at the end of the input.
fn read_message(input: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length.unwrap_or(0)];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_message(output: &mut impl Write, message: &Value) -> std::io::Result<()> {
    let body = message.to_string();
    write!(output, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A project whose only query is `SELECT name FROM users`.
    fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(
            &root.join("typedpg.config.json"),
            r#"{ "sql": { "include": ["q/*.sql"], "emit": "none" } }"#,
        );
        write(
            &root.join("migrations/0001.sql"),
            "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);",
        );
        write(
            &root.join("q/users.sql"),
            "/* @name byId */\nSELECT name FROM users WHERE id = $id;\n",
        );
        (dir, root)
    }

    fn compile(state: &mut State, root: &Path) -> Value {
        handle(
            "typedpg/compile",
            &json!({
                "config": root.join("typedpg.config.json").display().to_string(),
                "file": root.join("q/users.sql").display().to_string(),
            }),
            state,
        )
        .unwrap()
    }

    #[test]
    fn compile_returns_the_module_and_fills_the_cache() {
        let (_dir, root) = project();
        let mut state = State::default();
        let result = compile(&mut state, &root);
        let code = result["code"].as_str().unwrap();
        assert!(code.contains("export const byId = createQuery("), "{code}");
        assert_eq!(result["diagnostics"], json!([]));
        let dependencies: Vec<&str> = result["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap())
            .collect();
        assert!(
            dependencies
                .iter()
                .any(|d| d.ends_with("typedpg.config.json")),
            "{dependencies:?}"
        );
        assert!(
            dependencies.iter().any(|d| d.ends_with("0001.sql")),
            "{dependencies:?}"
        );

        // The entry `typedpg build` would have written: its key and module.
        let config = Config::load(&root.join("typedpg.config.json")).unwrap();
        let identity = cache::schema_identity(&config).unwrap();
        let path = root.join("q/users.sql");
        let content = fs::read_to_string(&path).unwrap();
        let key = cache::key(&config, &identity, &path, &content);
        let dir = cache::dir(&config);
        assert!(cache::exists(&dir, &key));
        assert_eq!(
            fs::read_to_string(dir.join(format!("{key}.js"))).unwrap(),
            code
        );
    }

    #[test]
    fn a_second_compile_uses_the_new_content() {
        let (_dir, root) = project();
        let mut state = State::default();
        let first = compile(&mut state, &root);
        assert!(
            first["code"]
                .as_str()
                .unwrap()
                .contains("SELECT name FROM users"),
            "{first}"
        );
        write(
            &root.join("q/users.sql"),
            "/* @name byId */\nSELECT id FROM users WHERE id = $id;\n",
        );
        let second = compile(&mut state, &root);
        let code = second["code"].as_str().unwrap();
        assert!(code.contains("SELECT id FROM users"), "{code}");
        assert!(!code.contains("SELECT name FROM users"), "{code}");
    }

    #[test]
    fn a_changed_migration_invalidates_the_schema() {
        let (_dir, root) = project();
        // The query reads a column the table does not have yet.
        write(
            &root.join("q/users.sql"),
            "/* @name email */\nSELECT email FROM users;\n",
        );
        let mut state = State::default();
        let before = compile(&mut state, &root);
        assert_eq!(before["diagnostics"].as_array().unwrap().len(), 1);
        assert!(
            before["code"]
                .as_str()
                .unwrap()
                .contains("createBrokenQuery("),
            "{before}"
        );
        // Rewrite the migration: the identity changes, the schema is built
        // again, and the query compiles.
        write(
            &root.join("migrations/0001.sql"),
            "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL, email text NOT NULL);",
        );
        let after = compile(&mut state, &root);
        assert_eq!(after["diagnostics"], json!([]));
        let code = after["code"].as_str().unwrap();
        assert!(code.contains("export const email = createQuery("), "{code}");
        assert!(!code.contains("createBrokenQuery("), "{code}");
    }

    #[test]
    fn a_broken_query_reports_a_diagnostic_and_fills_no_entry() {
        let (_dir, root) = project();
        write(
            &root.join("q/users.sql"),
            "/* @name typo */\nSELECT nmae FROM users;\n",
        );
        let mut state = State::default();
        let result = compile(&mut state, &root);
        let diagnostics = result["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 1, "{result}");
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic["file"], "q/users.sql");
        assert_eq!(diagnostic["line"], 2);
        assert_eq!(diagnostic["col"], 8);
        assert!(
            diagnostic["message"]
                .as_str()
                .unwrap()
                .starts_with("column \"nmae\" does not exist"),
            "{diagnostic}"
        );
        let config = Config::load(&root.join("typedpg.config.json")).unwrap();
        let identity = cache::schema_identity(&config).unwrap();
        let path = root.join("q/users.sql");
        let content = fs::read_to_string(&path).unwrap();
        let key = cache::key(&config, &identity, &path, &content);
        assert!(!cache::exists(&cache::dir(&config), &key));
    }

    #[test]
    fn a_file_outside_the_project_compiles_to_nothing() {
        let (_dir, root) = project();
        // It exists, but `sql.include` does not name it.
        write(&root.join("other.sql"), "/* @name x */ SELECT 1;\n");
        let mut state = State::default();
        let result = handle(
            "typedpg/compile",
            &json!({
                "config": root.join("typedpg.config.json").display().to_string(),
                "file": root.join("other.sql").display().to_string(),
            }),
            &mut state,
        )
        .unwrap();
        assert_eq!(result["code"], Value::Null);
        assert_eq!(result["diagnostics"], json!([]));
    }

    #[test]
    fn snippets_are_dropped() {
        let m = "column \"nmae\" does not exist\n  ╭────\n1 │ SELECT nmae\n  ╰────\n  help: Perhaps you meant \"name\".";
        assert_eq!(
            without_snippet(m),
            "column \"nmae\" does not exist\nhelp: Perhaps you meant \"name\"."
        );
    }

    #[test]
    fn framing_round_trips() {
        let mut out = Vec::new();
        write_message(&mut out, &json!({ "a": "é" })).unwrap();
        write_message(&mut out, &json!([1])).unwrap();
        let mut input = BufReader::new(out.as_slice());
        assert_eq!(
            read_message(&mut input).unwrap().unwrap(),
            "{\"a\":\"é\"}".as_bytes()
        );
        assert_eq!(read_message(&mut input).unwrap().unwrap(), b"[1]");
        assert_eq!(read_message(&mut input).unwrap(), None);
    }
}
