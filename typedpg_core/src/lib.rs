//! Internal shared core for the `typedpg` ecosystem.
//!
//! **Users should depend on the `typedpg` crate, not on `typedpg_core`
//! directly.** This crate contains foundational types shared between the
//! runtime (`typedpg`) and the proc macro (`typedpg_macros`).
//!
//! Kept intentionally small so that depending on it does not pull in heavy
//! compile-time-only dependencies (like the SQL parser used by the analyzer).
//!
//! # Modules
//!
//! - [`config`] -- Parses `[package.metadata.typedpg]` from `Cargo.toml`.
//! - [`build`] -- Build-script helper that re-runs `sql!` when migrations change.
//! - [`qualified_name`] -- PostgreSQL schema-qualified identifiers.

pub mod build;
pub mod config;
mod keywords;
mod qualified_name;

pub use qualified_name::{ParseQualifiedNameError, QualifiedName, quote_identifier};
