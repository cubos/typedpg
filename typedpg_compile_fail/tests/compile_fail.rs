//! Checks every case binary of `fixture/` and compares the errors rustc
//! reports for it with `fixture/expected/<case>.stderr`. See the crate docs
//! for the layout; run with `BLESS=1` to rewrite the snapshots.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// The toolchain the snapshots are taken with (see the comment where it
/// runs).
const TOOLCHAIN: &str = "1.99.0";

fn bless() -> bool {
    ["BLESS", "TYPEDPG_BLESS"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty() && s != "0"))
}

#[test]
fn compile_fail_snapshots() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = crate_dir.parent().expect("workspace root");
    let fixture = crate_dir.join("fixture");
    let expected_dir = fixture.join("expected");

    // The fixture is a workspace of its own: give it the main lockfile so
    // it builds the exact dependency versions the workspace does (cargo
    // drops the entries it does not need).
    let lock = std::fs::read(workspace.join("Cargo.lock")).expect("read Cargo.lock");
    let fixture_lock = fixture.join("Cargo.lock");
    if std::fs::read(&fixture_lock).ok().as_deref() != Some(&lock[..]) {
        std::fs::write(&fixture_lock, &lock).expect("write fixture Cargo.lock");
    }

    let cases = case_names(&fixture.join("src/bin"));
    assert!(!cases.is_empty(), "no cases under fixture/src/bin");

    let target_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("compile-fail");
    // rustc's own help and note lines change between releases, so the
    // snapshots are checked with one pinned toolchain, whatever runs the
    // tests: bump TOOLCHAIN (and CI's install step) and re-bless together.
    let output = Command::new("rustup")
        .args(["run", TOOLCHAIN, "cargo"])
        .args(["check", "--bins", "--keep-going", "--message-format=json"])
        .arg("--manifest-path")
        .arg(fixture.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .current_dir(&fixture)
        // A caller's flags (e.g. `-D warnings`) would turn the cases'
        // harmless warnings into errors and change every snapshot.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTC")
        .env_remove("RUSTC_WRAPPER")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "run `rustup run {TOOLCHAIN} cargo check` on the fixture ({e}); install the \
                 pinned toolchain with `rustup toolchain install {TOOLCHAIN} --profile minimal`"
            )
        });
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("is not installed"),
        "the pinned toolchain {TOOLCHAIN} is not installed: run `rustup toolchain install \
         {TOOLCHAIN} --profile minimal`"
    );

    let mut errors: BTreeMap<String, String> = BTreeMap::new();
    let mut foreign = String::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg["reason"] != "compiler-message" || msg["message"]["level"] != "error" {
            continue;
        }
        let rendered = msg["message"]["rendered"].as_str().unwrap_or_default();
        let is_bin = msg["target"]["kind"]
            .as_array()
            .is_some_and(|k| k.iter().any(|k| k == "bin"));
        let name = msg["target"]["name"].as_str().unwrap_or_default();
        if is_bin && cases.contains(&name.to_owned()) {
            errors
                .entry(name.to_owned())
                .or_default()
                .push_str(&normalize(rendered, &fixture, workspace));
        } else {
            foreign.push_str(rendered);
        }
    }
    assert!(
        foreign.is_empty(),
        "errors outside the case binaries (the fixture itself is broken):\n{foreign}\n\
         cargo stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut failures = Vec::new();
    for case in &cases {
        let actual = errors.remove(case).unwrap_or_default();
        let path = expected_dir.join(format!("{case}.stderr"));
        if case.starts_with("pass_") {
            if !actual.is_empty() {
                failures.push(format!("{case}: expected to compile, got:\n{actual}"));
            }
            continue;
        }
        if actual.is_empty() {
            failures.push(format!("{case}: expected a compile error, got none"));
            continue;
        }
        if bless() {
            std::fs::create_dir_all(&expected_dir).expect("create expected/");
            std::fs::write(&path, &actual).expect("write snapshot");
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(expected) if expected == actual => {}
            Ok(expected) => failures.push(format!(
                "{case}: errors differ from {}\n{}",
                path.display(),
                diff(&expected, &actual)
            )),
            Err(_) => failures.push(format!(
                "{case}: no snapshot at {} — run with BLESS=1. Actual:\n{actual}",
                path.display()
            )),
        }
    }

    // A snapshot whose case was removed or renamed.
    if let Ok(entries) = std::fs::read_dir(&expected_dir) {
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            let stem = file.strip_suffix(".stderr").unwrap_or(&file);
            if !cases.iter().any(|c| c == stem && !c.starts_with("pass_")) {
                if bless() {
                    std::fs::remove_file(entry.path()).expect("remove stale snapshot");
                } else {
                    failures.push(format!("stale snapshot without a case: {file}"));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} compile-fail case(s) failed (re-run with BLESS=1 to accept the new output):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn case_names(bin_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(bin_dir)
        .expect("read fixture/src/bin")
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            (path.extension()? == "rs").then(|| path.file_stem()?.to_str().map(str::to_owned))?
        })
        .collect();
    names.sort();
    names
}

/// Make the rendered diagnostics independent of the machine: absolute paths
/// become `$WORKSPACE/…`, `$CARGO/…` (registry sources) or `$RUST/…` (the
/// standard library), and positions in those files — which move with every
/// dependency release — become `LL:CC`.
///
/// rustc's `= help:` lists of "other types implementing the trait" are
/// dropped: they enumerate impls from the whole dependency graph, so any
/// dependency bump would change them.
fn normalize(rendered: &str, fixture: &Path, workspace: &Path) -> String {
    let fixture = format!("{}/", fixture.display());
    let workspace = format!("{}/", workspace.display());
    let mut out = String::with_capacity(rendered.len());
    let mut in_impl_list = false;
    for line in rendered.lines() {
        let trimmed = line.trim_start();
        if in_impl_list {
            if line.starts_with(' ') && !trimmed.starts_with("= ") && !trimmed.starts_with('|') {
                continue;
            }
            in_impl_list = false;
        }
        if trimmed.starts_with("= help:") && trimmed.ends_with(':') {
            in_impl_list = true;
            continue;
        }
        let line = line
            .replace(&fixture, "")
            .replace(&workspace, "$WORKSPACE/");
        out.push_str(&normalize_location(&line));
        out.push('\n');
    }
    out
}

fn normalize_location(line: &str) -> String {
    let Some(idx) = line.find("--> ").or_else(|| line.find("::: ")) else {
        return line.to_owned();
    };
    let (head, loc) = line.split_at(idx + 4);
    let path = loc.split(':').next().unwrap_or(loc);
    let external = if let Some(rest) = registry_relative(path) {
        format!("$CARGO/{rest}")
    } else if let Some(rest) = path.strip_prefix("/rustc/") {
        format!("$RUST/{}", rest.split_once('/').map_or(rest, |(_, r)| r))
    } else if path.starts_with("$WORKSPACE/") {
        path.to_owned()
    } else {
        return line.to_owned();
    };
    format!("{head}{external}:LL:CC")
}

/// `…/registry/src/<index>/<crate>-<ver>/src/x.rs` → `<crate>-<ver>/src/x.rs`.
fn registry_relative(path: &str) -> Option<&str> {
    let after = &path[path.find("/registry/src/")? + "/registry/src/".len()..];
    Some(after.split_once('/')?.1)
}

fn diff(expected: &str, actual: &str) -> String {
    let mut out = String::new();
    let (e, a): (Vec<_>, Vec<_>) = (expected.lines().collect(), actual.lines().collect());
    for i in 0..e.len().max(a.len()) {
        match (e.get(i), a.get(i)) {
            (Some(x), Some(y)) if x == y => out.push_str(&format!("  {x}\n")),
            (x, y) => {
                if let Some(x) = x {
                    out.push_str(&format!("- {x}\n"));
                }
                if let Some(y) = y {
                    out.push_str(&format!("+ {y}\n"));
                }
            }
        }
    }
    out
}
