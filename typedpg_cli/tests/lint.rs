//! `cargo typedpg lint` over `tests/lint_fixture`, in every format. The
//! expected documents are checked in; run with `BLESS=1` to rewrite them.
//!
//! The fixture is never built: the test only runs the CLI over its sources
//! and migrations.

use std::path::Path;
use std::process::Command;

fn bless() -> bool {
    ["BLESS", "TYPEDPG_BLESS"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty() && s != "0"))
}

#[test]
fn lint_fixture_reports_the_expected_documents() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = crate_dir.join("tests/lint_fixture");
    for (format, expected) in [
        ("human", "expected.txt"),
        ("json", "expected.json"),
        ("sarif", "expected.sarif"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-typedpg"))
            .args(["typedpg", "lint", "--manifest-path"])
            .arg(fixture.join("Cargo.toml"))
            .args(["--format", format])
            .output()
            .unwrap_or_else(|error| panic!("run `cargo-typedpg` for {format}: {error}"));
        let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{format}: expected exit status 1, got {:?}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            !stderr.contains(": error:"),
            "{format}: unexpected failure on stderr:\n{stderr}"
        );
        let path = fixture.join(expected);
        if bless() {
            std::fs::write(&path, &stdout).expect("write the expected document");
        } else {
            let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!(
                    "read {}: {error} — run with BLESS=1 to create it",
                    path.display()
                )
            });
            assert_eq!(
                stdout,
                expected,
                "{format}: stdout differs from {} — run with BLESS=1 to accept it",
                path.display()
            );
        }
    }
}
