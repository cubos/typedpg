//! Compile PostgreSQL's own sources — the slice `import/FILES` names, plus
//! `import/FILES.windows` on Windows — the generated services under
//! `postgres/typedpg/`, and the crate's C entry points under `csrc/`, and
//! expose the PostgreSQL release they implement (POSTGRES_VERSION).

use std::path::{Path, PathBuf};

fn main() {
    let postgres = PathBuf::from("postgres");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=import/FILES");
    println!("cargo:rerun-if-changed=import/FILES.windows");
    println!("cargo:rerun-if-changed=POSTGRES_VERSION");
    println!("cargo:rerun-if-changed=postgres/src");
    println!("cargo:rerun-if-changed=postgres/include");
    println!("cargo:rerun-if-changed=postgres/typedpg");

    let target = std::env::var("TARGET").unwrap_or_default();
    let windows = target.contains("windows");

    let include = postgres.join("include");
    let parser = postgres.join("src/backend/parser");
    let plpgsql = postgres.join("src/pl/plpgsql/src");

    let mut pg_sources = source_list(&postgres, "import/FILES");
    if libc_declares_strlcpy(&include, &target) {
        // postgres/src/port/strlcpy.c is only compiled by PostgreSQL where
        // the C library does not provide strlcpy (glibc < 2.38, Windows).
        // Where it does, compiling OpenBSD's definition anyway breaks under
        // libcs that make strlcpy a macro (<string.h> with _FORTIFY_SOURCE,
        // as macOS builds sometimes do), so decide with the same macro
        // port.h uses: HAVE_DECL_STRLCPY, as the target compiler sees it.
        pg_sources.retain(|source| source != &postgres.join("src/port/strlcpy.c"));
    }
    if windows {
        // Windows-only sources (see import/FILES.windows): open.c,
        // win32error.c, win32ntdll.c, win32gettimeofday.c and
        // win32pread.c/win32pwrite.c, with the Win32 APIs the other targets
        // get from their libc.
        pg_sources.extend(source_list(&postgres, "import/FILES.windows"));
    }
    pg_sources.extend(c_sources(&postgres.join("typedpg")));

    // `#option dump` in a PL/pgSQL body makes the compiler print the
    // function's tree to stdout (pl_comp.c → plpgsql_dumptree), which here
    // is the process running the analyzer. pl_comp.c is compiled with that
    // call renamed to typedpg_plpgsql_dumptree (csrc/pg/plpgsql_dump.c),
    // which prints nothing: the option is still accepted.
    let pl_comp = plpgsql.join("pl_comp.c");
    let before = pg_sources.len();
    pg_sources.retain(|source| source != &pl_comp);
    assert_eq!(before - 1, pg_sources.len(), "import/FILES names pl_comp.c");
    let mut quiet = cc::Build::new();
    quiet
        .file(&pl_comp)
        .include(&include)
        .include(&parser)
        .include(&plpgsql)
        .define("plpgsql_dumptree", "typedpg_plpgsql_dumptree")
        .warnings(false);
    configure(&mut quiet, &target);
    let pl_comp_objects = quiet.compile_intermediates();

    let mut build = cc::Build::new();
    build
        .files(&pg_sources)
        .objects(&pl_comp_objects)
        .include(&include)
        .include(&parser)
        .include(&plpgsql)
        .warnings(false);
    configure(&mut build, &target);
    build.compile("typedpg_postgres");

    // The crate's entry points are ours: compile them with the same flags.
    let mut shim_sources = c_sources(Path::new("csrc"));
    shim_sources.extend(c_sources(&Path::new("csrc").join("pg")));
    shim_sources.extend(c_sources(&Path::new("csrc").join("plpgsql")));
    let mut shim = cc::Build::new();
    shim.files(&shim_sources)
        .include(&include)
        .include(&parser)
        .include(&plpgsql)
        .include("csrc")
        .warnings(false);
    configure(&mut shim, &target);
    shim.compile("typedpg_shim");

    // What the imported sources call in the C library or the OS.
    if windows {
        // elog.c's Windows event log: RegisterEventSource() and
        // ReportEvent()/ReportEventW() (advapi32).  Everything else the
        // imported sources call (CreateFile, LoadLibrary, GetACP, ...) is in
        // kernel32, which Rust links already.  PostgreSQL's meson.build also
        // links ws2_32 and secur32 for the server's socket and SSPI code,
        // which this crate does not compile.
        println!("cargo:rustc-link-lib=advapi32");
    } else if !target.contains("apple") && !target.contains("musl") {
        // d2s.c, int8.c and others call libm (floor, rint, ...).  On macOS,
        // Windows and musl the functions come with the C library itself (and
        // asking for -lm on a musl target would find the build machine's
        // glibc libm.a).
        println!("cargo:rustc-link-lib=m");
    }

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
             /// [`PG_VERSION`] as `PG_VERSION_NUM` (`190000` for 19.0).\n\
             pub const PG_VERSION_NUM: i32 = {num};\n",
        ),
    )
    .expect("writing version.rs");
}

/// The C compiler flags every target needs, per compiler and OS.
fn configure(build: &mut cc::Build, target: &str) {
    let msvc = target.contains("msvc");
    let windows = target.contains("windows");
    if msvc {
        // PostgreSQL's C99/C11: what meson.build asks cl.exe for.
        build.std("c11");
        // What PostgreSQL's meson.build passes to cl.exe, plus the warnings
        // its CRT otherwise emits for POSIX names (`open`, `strdup`, ...),
        // which PostgreSQL uses on purpose.
        for define in [
            "WIN32",
            "WINDOWS",
            "__WINDOWS__",
            "__WIN32__",
            "_CRT_SECURE_NO_DEPRECATE",
            "_CRT_NONSTDC_NO_DEPRECATE",
            "_CRT_SECURE_NO_WARNINGS",
        ] {
            build.define(define, None);
        }
    } else {
        // PostgreSQL's C is written for C11 with GCC extensions.
        build.std("gnu11");
        // What PostgreSQL's configure and libpg_query both compile with:
        // signed overflow wraps and differently-typed pointers may alias.
        build.flag_if_supported("-fno-strict-aliasing");
        build.flag_if_supported("-fwrapv");
        if target.starts_with("aarch64") && target.contains("musl") {
            // musl-gcc's aarch64 atomics call libgcc's out-of-line helpers
            // (__aarch64_swp4_sync, ...), which the musl toolchain the
            // release builds with doesn't link: inline them.
            build.flag_if_supported("-mno-outline-atomics");
        }
        if windows {
            // MinGW: defined before <time.h> for localtime_r() and others.
            build.define("WIN32", None);
            build.define("_POSIX_C_SOURCE", None);
        }
    }
    if target.contains("linux") {
        // strlcpy/strlcat and the other _GNU_SOURCE declarations; the
        // imported pg_config.h decides which the libc really has.
        build.define("_GNU_SOURCE", None);
    }
    if windows {
        // PostgreSQL's stand-ins for the Windows SDK's missing headers
        // (unistd.h, utime.h, sys/time.h, arpa/inet.h, ...) and the MSVC
        // replacements (dirent.h, ...).  meson.build adds both directories
        // the same way.  These live under postgres/include/port/, which is
        // also the include root PostgreSQL's own sources use.
        build
            .include("postgres/include/port/win32")
            .include("postgres/include/port/win32_msvc");
    }
}

/// Whether the target's C library declares strlcpy, i.e. whether port.h's
/// `#if !HAVE_DECL_STRLCPY` prototype (and PostgreSQL's own build of
/// src/port/strlcpy.c) is off.
///
/// musl, macOS and Windows are fixed; glibc only gained strlcpy in 2.38, so
/// the version the target compiler will see decides.  It is asked through the
/// preprocessor, with the same macro port.h tests (the imported pg_config.h
/// computes it from __GLIBC__), rather than guessed from the target triple.
fn libc_declares_strlcpy(include: &Path, target: &str) -> bool {
    if target.contains("windows") {
        return false;
    }
    if target.contains("apple") || target.contains("musl") {
        return true;
    }
    let probe = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("strlcpy_probe.c");
    std::fs::write(&probe, "#include \"pg_config.h\"\n").expect("writing the strlcpy probe");
    let mut build = cc::Build::new();
    build.include(include);
    configure(&mut build, target);
    let Ok(compiler) = build.try_get_compiler() else {
        return false;
    };
    let mut cmd = compiler.to_command();
    let output = cmd
        .arg("-E")
        .arg("-dM")
        .arg("-I")
        .arg(include)
        .arg(&probe)
        .output();
    let Ok(output) = output else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim_end() == "#define HAVE_DECL_STRLCPY 1")
}

/// The entries of one of import/'s source lists, at their postgres/ path.
fn source_list(postgres: &Path, list: &str) -> Vec<PathBuf> {
    let text = std::fs::read_to_string(list).unwrap_or_else(|e| panic!("reading {list}: {e}"));
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .map(|file| postgres.join(file))
        .collect()
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
