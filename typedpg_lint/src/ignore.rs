//! `lint.ignore`: the paths whose findings the report drops.
//!
//! Patterns are globs relative to the project root, with forward slashes —
//! the manifest's directory for Rust, the config file's for TypeScript.
//! A matching query file's findings are dropped; a matching migration file
//! is linted schema-only, so its schema findings (placed on the statements
//! that created the objects) survive while its statement rules are skipped.
//! Only findings are dropped: an ignored file that does not compile still
//! fails the run, and its queries are still analyzed and counted.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// The compiled `lint.ignore` patterns.
#[derive(Debug, Clone)]
pub struct IgnoreSet {
    globs: GlobSet,
}

impl IgnoreSet {
    /// Compile `patterns`; an invalid glob is an error naming it.
    pub fn new(patterns: &[String]) -> Result<IgnoreSet, String> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|error| format!("invalid lint.ignore pattern `{pattern}`: {error}"))?;
            builder.add(glob);
        }
        let globs = builder
            .build()
            .map_err(|error| format!("invalid lint.ignore patterns: {error}"))?;
        Ok(IgnoreSet { globs })
    }

    /// Whether findings in `path` (relative to the project root, with
    /// forward slashes) are dropped.
    pub fn is_ignored(&self, path: &str) -> bool {
        self.globs.is_match(path.replace('\\', "/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignore(patterns: &[&str]) -> IgnoreSet {
        IgnoreSet::new(&patterns.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn patterns_match_paths_relative_to_the_root() {
        let set = ignore(&["tests/**", "**/fixtures/*.sql"]);
        assert!(set.is_ignored("tests/ignored.rs"));
        assert!(set.is_ignored("tests/a/b/query.sql"));
        assert!(set.is_ignored("src/fixtures/users.sql"));
        assert!(set.is_ignored("src/a/fixtures/users.sql"));
        assert!(!set.is_ignored("src/main.rs"));
        assert!(!set.is_ignored("src/a/fixtures/nested/users.sql"));
        assert!(!set.is_ignored("tests"));
    }

    #[test]
    fn backslashes_are_forward_slashes() {
        let set = ignore(&["tests/**"]);
        assert!(set.is_ignored("tests\\ignored.rs"));
    }

    #[test]
    fn an_invalid_pattern_is_an_error_naming_it() {
        let error = IgnoreSet::new(&["tests/[".to_owned()]).unwrap_err();
        assert!(
            error.contains("invalid lint.ignore pattern `tests/[`"),
            "{error}"
        );
    }
}
