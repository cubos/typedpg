//! The message both runners report for an applied migration edited since.
//!
//! A single wording matters: `typedpg migrate` (the npm package) and
//! `cargo typedpg migrate` manage the same database, so they must explain
//! drift, the diff and how to record an intentional edit the same way.
//! The message lives here, and `typedpg_wasm` exposes it to the npm runner.

/// The drift message: the migration's name, a unified diff of what was
/// applied against the file (when its text is stored), and how to make an
/// intentional edit stop being drift. The error also says how to downgrade
/// drift to a warning.
pub fn drift_message(
    name: &str,
    stored_sql: Option<&str>,
    file_sql: &str,
    fail_on_drift: bool,
) -> String {
    let mut message = format!("migration '{name}' has been modified since it was applied:");
    if let Some(stored_sql) = stored_sql {
        let file_name = format!("{name}.sql");
        let diff = super::diff::unified_diff(stored_sql, file_sql, &file_name);
        message.push('\n');
        message.push_str(diff.trim_end());
    }
    message.push_str(&format!(
        "\nIf the change is intentional (it does not need to run again), record the file as \
         applied with `typedpg migrate accept {name}`; otherwise restore the file and add a new \
         migration."
    ));
    if fail_on_drift {
        // Both runners read their own configuration format: name both, so
        // the message fits whichever one the reader is using.
        message.push_str(
            "\nSet fail_on_drift = false ([package.metadata.typedpg.migrations] in Cargo.toml) \
             or \"failOnDrift\": false (\"migrations\" in typedpg.config.json) to downgrade this \
             to a warning.",
        );
    }
    message
}

#[cfg(test)]
mod tests {
    use super::drift_message;

    #[test]
    fn an_edited_migration_gets_a_unified_diff() {
        let message = drift_message(
            "0001_a",
            Some("CREATE TABLE a (\n    id INT\n);"),
            "CREATE TABLE a (\n    id BIGINT\n);",
            true,
        );
        assert!(
            message.starts_with("migration '0001_a' has been modified since it was applied:\n")
        );
        for needle in [
            "--- applied",
            "+++ 0001_a.sql",
            "@@ -1,3 +1,3 @@",
            "-    id INT",
            "+    id BIGINT",
            "typedpg migrate accept 0001_a",
            "fail_on_drift = false",
            "\"failOnDrift\": false",
        ] {
            assert!(
                message.contains(needle),
                "missing {needle:?} in:\n{message}"
            );
        }
    }

    /// Without the stored text there is nothing to diff, and the warning
    /// path does not advertise how to downgrade a setting already off.
    #[test]
    fn without_stored_text_the_diff_and_the_downgrade_hint_are_omitted() {
        let message = drift_message("0001_a", None, "SELECT 1;", false);
        assert!(!message.contains("@@"), "{message}");
        assert!(!message.contains("fail_on_drift"), "{message}");
        assert!(
            message.contains("typedpg migrate accept 0001_a"),
            "{message}"
        );
    }
}
