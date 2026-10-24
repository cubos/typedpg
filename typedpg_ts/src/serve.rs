//! `typedpg serve`: the content mapper TypeScript 7.1 runs for `.sql`
//! files (`contentMappers` in tsconfig.json, `tsc --runExternalCode`).
//!
//! JSON-RPC over stdin / stdout with LSP's `Content-Length` framing;
//! TypeScript sends every request: `initialize`, then `openProject` per
//! project, `transform` per file, `closeProject`. A file is transformed
//! into the module `emit: "ts"` would write for it — the same types,
//! `createBrokenQuery` for a query with an error — each export's name
//! mapped to its `@name`, and the file's errors as diagnostics at their
//! place in it.
//!
//! The project's typedpg config is the mapper's `"config"` option (a path
//! relative to tsconfig.json), else the nearest `typedpg.config.json` above
//! tsconfig.json. Its identity — what TypeScript keys its incremental state
//! on — is the version and the config and migrations' hash, and the files
//! it watches are the config file and the migrations.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::cache;
use crate::config::{CONFIG_FILE, Config};
use crate::project::Project;

/// Diagnostic codes, shown as `typedpg<code>`.
const CODE_QUERY: u32 = 1;
const CODE_PROJECT: u32 = 2;

struct Opened {
    /// The project, or why there is none (reported on each file).
    project: Result<Project, String>,
}

pub fn run() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut projects: HashMap<String, Opened> = HashMap::new();
    while let Some(message) = read_message(&mut input).map_err(|e| e.to_string())? {
        let Ok(message) = serde_json::from_slice::<Value>(&message) else {
            continue;
        };
        // Notifications: none is defined; nothing to answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let response = match handle(method, &params, &mut projects) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message },
            }),
        };
        write_message(&mut output, &response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

type RpcError = (i64, String);

fn handle(
    method: &str,
    params: &Value,
    projects: &mut HashMap<String, Opened>,
) -> Result<Value, RpcError> {
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
            projects.insert(handle, opened);
            Ok(result)
        }
        "closeProject" => {
            projects.remove(str_param("projectHandle")?);
            Ok(Value::Null)
        }
        "transform" => {
            let handle = str_param("projectHandle")?;
            let file = PathBuf::from(str_param("fileName")?);
            let content = str_param("content")?;
            let opened = projects
                .get_mut(handle)
                .ok_or_else(|| (-32602, format!("unknown project `{handle}`")))?;
            Ok(transform(opened, &file, content))
        }
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
