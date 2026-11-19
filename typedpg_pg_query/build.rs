//! Compile the vendored libpg_query and expose the PostgreSQL release it
//! is brought to (POSTGRES_VERSION).

use std::path::Path;

include!("patches.rs");

/// libpg_query's PostgreSQL headers, compiled from a copy (see main).
const HEADERS: &str = "src/postgres/include";

/// Copy the directory tree `from` to `to`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap_or_else(|e| panic!("creating {}: {e}", to.display()));
    for entry in
        std::fs::read_dir(from).unwrap_or_else(|e| panic!("reading {}: {e}", from.display()))
    {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target)
                .unwrap_or_else(|e| panic!("copying {}: {e}", entry.path().display()));
        }
    }
}

fn main() {
    let lib = Path::new("libpg_query");
    assert!(
        lib.join("pg_query.h").exists(),
        "libpg_query sources missing: run `git submodule update --init`"
    );
    println!("cargo:rerun-if-changed=libpg_query");

    let glob = |dir: &Path| -> Vec<std::path::PathBuf> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "c"))
            .collect();
        files.sort();
        files
    };

    // Sources with a patch (patches.rs) are compiled from a patched copy.
    println!("cargo:rerun-if-changed=patches.rs");
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=POSTGRES_VERSION");
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // With LF line endings, as the patches are written: a checkout with
    // `core.autocrlf` (Windows) has CRLF ones.
    let read = |file: &str| -> String {
        std::fs::read_to_string(lib.join(file))
            .unwrap_or_else(|e| panic!("reading libpg_query/{file}: {e}"))
            .replace("\r\n", "\n")
    };
    let mut patched: std::collections::BTreeMap<&str, String> = std::collections::BTreeMap::new();
    for patch in PATCHES {
        let text = patched
            .entry(patch.file)
            .or_insert_with(|| read(patch.file));
        let found = text.matches(patch.find).count();
        assert!(
            found == 1,
            "patch for libpg_query/{} matches {found} times (expected once); libpg_query \
             changed there, so re-derive it (patches.rs). The patch: {}",
            patch.file,
            patch.why
        );
        *text = text.replacen(patch.find, patch.replace, 1);
    }

    // The PostgreSQL release typedpg targets. libpg_query's sources come
    // from an earlier minor release of the same major, which patches.rs
    // brings to it where parsing differs; its version defines are rewritten
    // to it, so the parse results' `version` says the same.
    let postgres = std::fs::read_to_string("POSTGRES_VERSION").expect("reading POSTGRES_VERSION");
    let postgres = postgres.trim().to_owned();
    let release = |version: &str| -> (u32, u32) {
        version
            .split_once('.')
            .and_then(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?)))
            .unwrap_or_else(|| panic!("{version:?} is not a PostgreSQL release (`18.7`)"))
    };
    let (major, minor) = release(&postgres);
    let upstream = define(&read("pg_query.h"), "PG_VERSION");
    let (upstream_major, upstream_minor) = release(upstream.trim_matches('"'));
    assert!(
        upstream_major == major && upstream_minor <= minor,
        "POSTGRES_VERSION is {postgres}, but libpg_query's sources are PostgreSQL {upstream}: a \
         target must be a later minor release of their major (see README.md)"
    );
    let num = major * 10000 + minor;
    for (file, defines) in [
        (
            "pg_query.h",
            vec![
                ("PG_VERSION", format!("\"{postgres}\"")),
                ("PG_VERSION_NUM", num.to_string()),
            ],
        ),
        (
            "src/postgres/include/pg_config.h",
            vec![
                ("PACKAGE_STRING", format!("\"PostgreSQL {postgres}\"")),
                ("PACKAGE_VERSION", format!("\"{postgres}\"")),
                ("PG_MINORVERSION_NUM", minor.to_string()),
                ("PG_VERSION", format!("\"{postgres}\"")),
                ("PG_VERSION_NUM", num.to_string()),
                (
                    "PG_VERSION_STR",
                    format!("\"PostgreSQL {postgres} (libpg_query)\""),
                ),
            ],
        ),
    ] {
        let text = patched.entry(file).or_insert_with(|| read(file));
        for (name, value) in defines {
            let prefix = format!("#define {name} ");
            let lines: Vec<&str> = text.lines().filter(|l| l.starts_with(&prefix)).collect();
            assert_eq!(
                lines.len(),
                1,
                "libpg_query/{file} defines {name} {} times",
                lines.len()
            );
            *text = text.replacen(lines[0], &format!("{prefix}{value}"), 1);
        }
    }
    // A patched header can't just be shadowed through the include path:
    // its neighbours `#include "…"` it, which looks in their own directory
    // first. The headers are compiled from a copy of the include tree, with
    // the patched headers written into it.
    let includes = out_dir.join("patched").join(HEADERS);
    if includes.exists() {
        std::fs::remove_dir_all(&includes).expect("clearing the header copy");
    }
    copy_tree(&lib.join(HEADERS), &includes);
    let mut replacements = std::collections::HashMap::new();
    for (file, text) in &patched {
        let copy = out_dir.join("patched").join(file);
        std::fs::create_dir_all(copy.parent().expect("a parent")).expect("creating patched/");
        std::fs::write(&copy, text).expect("writing a patched source");
        replacements.insert(lib.join(file), copy);
    }
    let source = |p: std::path::PathBuf| replacements.get(&p).cloned().unwrap_or(p);

    let mut build = cc::Build::new();
    build
        .files(glob(&lib.join("src")).into_iter().map(source))
        .files(glob(&lib.join("src/postgres")).into_iter().map(source))
        .file(lib.join("vendor/xxhash/xxhash.c"))
        // Protobuf, through upb (libpg_query's Makefile: UPB_SRC_FILES).
        .file(lib.join("vendor/upb/upb.c"))
        .file(lib.join("vendor/upb/third_party/utf8_range/utf8_range.c"))
        .file(lib.join("protobuf/pg_query.upb_minitable.c"))
        // The catalog hooks (patched lookups call into them).
        .file("csrc/catalog.c")
        .include("csrc")
        // The patched pg_query.h, ahead of the original beside it.
        .include(out_dir.join("patched"))
        .include(lib)
        // A patched copy's `#include "…"` of its upstream neighbours.
        .include(lib.join("src"))
        .include(lib.join("src/postgres"))
        .include(lib.join("vendor"))
        .include(lib.join("vendor/upb"))
        .include(lib.join("vendor/upb/third_party/utf8_range"))
        .include(&includes)
        .include(lib.join("src/include"))
        // libpg_query's own warnings are its maintainers' concern.
        .warnings(false);
    // What PostgreSQL's C is written for (libpg_query's Makefile, and
    // PostgreSQL's configure): signed overflow wraps, and pointers of
    // different types may alias. MSVC does neither optimization.
    build.flag_if_supported("-fwrapv");
    build.flag_if_supported("-fno-strict-aliasing");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        build.include(includes.join("port/win32"));
        if target.contains("msvc") {
            build.include(includes.join("port/win32_msvc"));
        }
    }
    build.compile("pg_query");

    let out = out_dir.join("version.rs");
    std::fs::write(
        out,
        format!(
            "/// The PostgreSQL release this parser implements (POSTGRES_VERSION).\n\
             pub const PG_VERSION: &str = \"{postgres}\";\n\
             /// [`PG_VERSION`] as `PG_VERSION_NUM` (`180007` for 18.7).\n\
             pub const PG_VERSION_NUM: i32 = {num};\n",
        ),
    )
    .expect("writing version.rs");
}

/// The value of `#define name value` in `header`.
fn define(header: &str, name: &str) -> String {
    header
        .lines()
        .find_map(|l| l.strip_prefix(&format!("#define {name} ")))
        .unwrap_or_else(|| panic!("{name} not defined"))
        .trim()
        .to_owned()
}
