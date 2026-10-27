//! The `typedpg serve` process: a compile without the TypeScript handshake,
//! `--idle-timeout`, and end of stdin.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_typedpg")
}

fn write_message(output: &mut impl Write, body: &str) {
    write!(output, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    output.flush().unwrap();
}

fn read_message(input: &mut impl BufRead) -> String {
    let mut length = None;
    loop {
        let mut line = String::new();
        assert!(input.read_line(&mut line).unwrap() > 0, "end of output");
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; length.expect("a Content-Length")];
    input.read_exact(&mut body).unwrap();
    String::from_utf8(body).unwrap()
}

/// Wait for the child, failing after `timeout` instead of hanging the run.
fn wait(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the child did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn compile_needs_no_handshake_and_answers_in_its_framing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
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

    let mut child = Command::new(binary())
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "typedpg/compile",
        "params": {
            "config": root.join("typedpg.config.json").display().to_string(),
            "file": root.join("q/users.sql").display().to_string(),
        },
    });
    write_message(&mut stdin, &request.to_string());
    let response: serde_json::Value = serde_json::from_str(&read_message(&mut stdout)).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let result = &response["result"];
    assert!(
        result["code"]
            .as_str()
            .unwrap()
            .contains("export const byId = createQuery("),
        "{result}"
    );
    assert_eq!(result["diagnostics"], serde_json::json!([]));

    // Closing stdin exits the process, after the request was answered.
    drop(stdin);
    let status = wait(&mut child, Duration::from_secs(20));
    assert!(status.success(), "{status:?}");
}

#[test]
fn the_idle_timeout_exits_the_process() {
    let start = Instant::now();
    let mut child = Command::new(binary())
        .args(["serve", "--idle-timeout", "0.2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let status = wait(&mut child, Duration::from_secs(20));
    assert!(status.success(), "{status:?}");
    assert!(start.elapsed() >= Duration::from_millis(150));
}

#[test]
fn end_of_stdin_exits_the_process() {
    let mut child = Command::new(binary())
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let status = wait(&mut child, Duration::from_secs(20));
    assert!(status.success(), "{status:?}");
}
