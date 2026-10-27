//! The pure helpers both migration runners share.
//!
//! The Rust runner (`typedpg::migrate`) and the npm one (`@cubos/typedpg`,
//! through `typedpg_wasm`) manage the same tracking table: they must parse
//! the same file names, split statements the same way, agree on what drift
//! is, and explain it with the same message. Everything here is free of I/O
//! and of database types, so `typedpg_core` stays buildable for
//! `wasm32-unknown-unknown`.
//!
//! - [`compare_versions`] / [`compare_migration_names`] — migration order.
//! - [`parse_migration_name`] — the `NNNN_description` rule.
//! - [`split_statements`] — a migration as the statements the runner sends.
//! - [`normalize`] / [`hash`] — the text drift detection compares.
//! - [`drift_message`] — the error (or warning) drift produces.
//! - [`locate_position`] — a server error's line and column in the file.
//! - [`machine`] — the whole runner as a sans-I/O state machine, driven by
//!   both runners.

use std::cmp::Ordering;

mod diff;
mod drift;
mod name;
mod normalize;
mod position;
mod split;

pub mod machine;

pub use drift::drift_message;
pub use name::{MigrationNameError, parse_migration_name};
pub use normalize::{HASH_VERSION, hash, normalize};
pub use position::locate_position;
pub use split::split_statements;

/// Compare two migration versions — the numeric prefix of their names — as
/// numbers: `2` before `10`.
///
/// The prefixes are decimal strings, compared by their value whatever their
/// length: a forty-digit prefix is still a number.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn norm(s: &str) -> &str {
        let trimmed = s.trim_start_matches('0');
        if trimmed.is_empty() { "0" } else { trimmed }
    }
    let (a, b) = (norm(a), norm(b));
    // More digits means a bigger number; the same count compares by value.
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Compare two migration names — `0001_create_users`, or the file name
/// `0001_create_users.sql` — by their version, then by their name.
///
/// The name settles the order of two migrations with the same version, so
/// that every reader of a directory (the runner, the macro, the compiler)
/// applies them in the same order.
pub fn compare_migration_names(a: &str, b: &str) -> Ordering {
    fn version(name: &str) -> &str {
        name.split('_').next().unwrap_or(name)
    }
    compare_versions(version(a), version(b)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::{compare_migration_names, compare_versions};

    #[test]
    fn versions_compare_as_numbers() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("2", "10"), Less);
        assert_eq!(compare_versions("10", "2"), Greater);
        assert_eq!(compare_versions("0002", "2"), Equal);
        assert_eq!(compare_versions("0002", "0002"), Equal);
        assert_eq!(compare_versions("0", "1"), Less);
        assert_eq!(compare_versions("9", "10"), Less);
        // Whatever the length.
        let huge = "9".repeat(40);
        assert_eq!(compare_versions(&huge, "10"), Greater);
        assert_eq!(compare_versions(&huge, &huge), Equal);
    }

    #[test]
    fn names_compare_by_version_then_by_name() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_migration_names("2_b", "10_c"), Less);
        assert_eq!(compare_migration_names("10_c", "2_b"), Greater);
        assert_eq!(compare_migration_names("1_a", "1_b"), Less);
        assert_eq!(compare_migration_names("1_b.sql", "1_c.sql"), Less);
    }
}
