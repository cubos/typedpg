//! Compile the vendored libpg_query and expose its PostgreSQL version.

use std::path::Path;

include!("patches.rs");

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
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let mut patched: std::collections::BTreeMap<&str, String> = std::collections::BTreeMap::new();
    for patch in PATCHES {
        let text = patched.entry(patch.file).or_insert_with(|| {
            std::fs::read_to_string(lib.join(patch.file))
                .unwrap_or_else(|e| panic!("reading libpg_query/{}: {e}", patch.file))
        });
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
        .file(lib.join("vendor/protobuf-c/protobuf-c.c"))
        .file(lib.join("vendor/xxhash/xxhash.c"))
        .file(lib.join("protobuf/pg_query.pb-c.c"))
        // The catalog hooks (patched lookups call into them).
        .file("csrc/catalog.c")
        .include("csrc")
        .include(lib)
        // A patched copy's `#include "…"` of its upstream neighbours.
        .include(lib.join("src"))
        .include(lib.join("src/postgres"))
        .include(lib.join("vendor"))
        .include(lib.join("src/postgres/include"))
        .include(lib.join("src/include"))
        // libpg_query's own warnings are its maintainers' concern.
        .warnings(false);
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        build.include(lib.join("src/postgres/include/port/win32"));
        if target.contains("msvc") {
            build.include(lib.join("src/postgres/include/port/win32_msvc"));
        }
    }
    build.compile("pg_query");

    // `#define PG_VERSION "18.4"` / `#define PG_VERSION_NUM 180004`.
    let header = std::fs::read_to_string(lib.join("pg_query.h")).expect("reading pg_query.h");
    let define = |name: &str| -> String {
        header
            .lines()
            .find_map(|l| l.strip_prefix(&format!("#define {name} ")))
            .unwrap_or_else(|| panic!("{name} not defined in pg_query.h"))
            .trim()
            .to_owned()
    };
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("version.rs");
    std::fs::write(
        out,
        format!(
            "/// The PostgreSQL release whose grammar this parser implements.\n\
             pub const PG_VERSION: &str = {};\n\
             /// [`PG_VERSION`] as `PG_VERSION_NUM` (`180004` for 18.4).\n\
             pub const PG_VERSION_NUM: i32 = {};\n",
            define("PG_VERSION"),
            define("PG_VERSION_NUM"),
        ),
    )
    .expect("writing version.rs");
}
