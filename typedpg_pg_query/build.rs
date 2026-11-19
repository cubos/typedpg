//! Compile PostgreSQL's own sources — the slice `import/FILES` names — the
//! generated services under `postgres/typedpg/`, and the crate's C entry
//! points under `csrc/pg/`, and expose the PostgreSQL release they implement
//! (POSTGRES_VERSION).

use std::path::{Path, PathBuf};

fn main() {
    let postgres = PathBuf::from("postgres");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=import/FILES");
    println!("cargo:rerun-if-changed=POSTGRES_VERSION");
    println!("cargo:rerun-if-changed=postgres/src");
    println!("cargo:rerun-if-changed=postgres/include");
    println!("cargo:rerun-if-changed=postgres/typedpg");

    let include = postgres.join("include");
    let parser = postgres.join("src/backend/parser");
    let plpgsql = postgres.join("src/pl/plpgsql/src");

    let mut pg_sources = Vec::new();
    let files = std::fs::read_to_string("import/FILES").expect("reading import/FILES");
    for file in files.lines().map(str::trim).filter(|l| !l.is_empty()) {
        pg_sources.push(postgres.join(file));
    }
    pg_sources.extend(c_sources(&postgres.join("typedpg")));

    let mut build = cc::Build::new();
    build
        .files(&pg_sources)
        .include(&include)
        .include(&parser)
        .include(&plpgsql)
        // PostgreSQL's C is written for C11 with GCC extensions.
        .std("gnu11")
        // What PostgreSQL's configure and libpg_query both compile with:
        // signed overflow wraps and differently-typed pointers may alias.
        .flag_if_supported("-fno-strict-aliasing")
        .flag_if_supported("-fwrapv");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("linux") {
        build.define("_GNU_SOURCE", None);
    }
    // The imported sources call the C library's math functions (d2s, int8).
    if !target.contains("windows") {
        println!("cargo:rustc-link-lib=m");
    }
    // PostgreSQL's own warnings are its maintainers' concern.
    build.warnings(false);
    build.compile("typedpg_postgres");

    // The crate's entry points are ours: compile them with warnings on.
    let mut shim_sources = c_sources(Path::new("csrc"));
    shim_sources.extend(c_sources(&Path::new("csrc").join("pg")));
    shim_sources.extend(c_sources(&Path::new("csrc").join("plpgsql")));
    let mut shim = cc::Build::new();
    shim.files(&shim_sources)
        .include(&include)
        .include(&parser)
        .include(&plpgsql)
        .include("csrc")
        .std("gnu11")
        .flag_if_supported("-fno-strict-aliasing")
        .flag_if_supported("-fwrapv")
        // PostgreSQL's headers warn under -Wall as much as its sources.
        .warnings(false);
    if target.contains("linux") {
        shim.define("_GNU_SOURCE", None);
    }
    shim.compile("typedpg_shim");

    let postgres_version =
        std::fs::read_to_string("POSTGRES_VERSION").expect("reading POSTGRES_VERSION");
    let postgres_version = postgres_version.trim().to_owned();
    let num = define(&include.join("pg_config.h"), "PG_VERSION_NUM")
        .parse::<i32>()
        .unwrap_or_else(|e| panic!("PG_VERSION_NUM in postgres/include/pg_config.h: {e}"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("version.rs");
    std::fs::write(
        out,
        format!(
            "/// The PostgreSQL release this parser implements (POSTGRES_VERSION).\n\
             pub const PG_VERSION: &str = \"{postgres_version}\";\n\
             /// [`PG_VERSION`] as `PG_VERSION_NUM` (`180007` for 18.7).\n\
             pub const PG_VERSION_NUM: i32 = {num};\n",
        ),
    )
    .expect("writing version.rs");
}

/// The `.c` files of a directory, sorted.
fn c_sources(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "c"))
        .collect();
    files.sort();
    files
}

/// The value of `#define NAME value` in `header`.
fn define(header: &Path, name: &str) -> String {
    std::fs::read_to_string(header)
        .unwrap_or_else(|e| panic!("reading {}: {e}", header.display()))
        .lines()
        .find_map(|l| l.strip_prefix(&format!("#define {name} ")))
        .unwrap_or_else(|| panic!("{name} not defined in {}", header.display()))
        .trim()
        .to_owned()
}
