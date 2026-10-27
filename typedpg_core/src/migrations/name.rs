//! Migration file-name parsing: `NNNN_description`.
//!
//! The numeric prefix orders the migrations and the whole stem is the key in
//! the tracking table, so both runners must accept exactly the same names —
//! and reject the others with the same wording. Shared with `typedpg_wasm`.

use std::fmt;

/// Why a migration file stem is not `NNNN_description`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationNameError {
    /// No `_` at all (`createusers`).
    MissingUnderscore,
    /// The part before the `_` is empty or not all digits (`abc_users`).
    NonNumericPrefix,
    /// Nothing after the `_` (`0001_`).
    EmptyDescription,
}

impl MigrationNameError {
    /// The message both runners report, naming the offending file.
    ///
    /// The path is passed by the caller: the Rust runner has a
    /// [`std::path::Path`], the npm runner the string it joined.
    pub fn message(&self, path: impl fmt::Display) -> String {
        match self {
            Self::MissingUnderscore => {
                format!("migration file does not follow NNNN_description.sql format: {path}")
            }
            Self::NonNumericPrefix => {
                format!("migration file does not have a numeric prefix (NNNN_...): {path}")
            }
            Self::EmptyDescription => {
                format!("migration file has no description after prefix: {path}")
            }
        }
    }
}

/// The version (the numeric prefix) of a migration file stem like
/// `0001_create_users`, or why the stem is malformed.
///
/// `description` is everything after the first `_`; it may itself contain
/// underscores.
pub fn parse_migration_name(stem: &str) -> Result<&str, MigrationNameError> {
    let (version, description) = stem
        .split_once('_')
        .ok_or(MigrationNameError::MissingUnderscore)?;
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return Err(MigrationNameError::NonNumericPrefix);
    }
    if description.is_empty() {
        return Err(MigrationNameError::EmptyDescription);
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::{MigrationNameError, parse_migration_name};

    #[test]
    fn the_version_is_the_prefix_before_the_first_underscore() {
        assert_eq!(parse_migration_name("0001_create_users"), Ok("0001"));
        assert_eq!(parse_migration_name("1_a"), Ok("1"));
        assert_eq!(parse_migration_name("20240101_1200_a_b"), Ok("20240101"));
    }

    #[test]
    fn each_way_of_malforming_a_stem_has_its_message() {
        let cases = [
            (
                "createusers",
                MigrationNameError::MissingUnderscore,
                "migration file does not follow NNNN_description.sql format: 0001.sql",
            ),
            (
                "abc_create_users",
                MigrationNameError::NonNumericPrefix,
                "migration file does not have a numeric prefix (NNNN_...): 0001.sql",
            ),
            (
                "_create_users",
                MigrationNameError::NonNumericPrefix,
                "migration file does not have a numeric prefix (NNNN_...): 0001.sql",
            ),
            (
                "0001_",
                MigrationNameError::EmptyDescription,
                "migration file has no description after prefix: 0001.sql",
            ),
        ];
        for (stem, kind, message) in cases {
            assert_eq!(parse_migration_name(stem), Err(kind));
            assert_eq!(kind.message("0001.sql"), message);
        }
    }
}
