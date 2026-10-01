//! Compile-fail snapshot tests for the `sql!`, `copy_in!`,
//! `embed_migrations!` and `#[derive(FromRow)]` macros.
//!
//! The macros read `[package.metadata.typedpg]` from the *calling* crate's
//! `Cargo.toml`, so the cases cannot be compiled as tests of this crate (or
//! inside a trybuild scratch project, whose generated manifest carries no
//! such metadata). They live instead in `fixture/`, a standalone crate with
//! its own config, migrations and one binary per case under `src/bin/`.
//!
//! `tests/compile_fail.rs` runs `cargo check --bins --keep-going` on the
//! fixture, groups the compiler's errors by binary, and compares each
//! case's rendered errors with `fixture/expected/<case>.stderr`. A case
//! named `pass_*` must compile without errors.
//!
//! ```text
//! cargo nextest run -p typedpg_compile_fail             # check
//! BLESS=1 cargo nextest run -p typedpg_compile_fail     # rewrite snapshots
//! ```
//!
//! Re-bless whenever a macro's wording changes on purpose (or a new rustc
//! renders a diagnostic differently), and review the snapshot diff.
