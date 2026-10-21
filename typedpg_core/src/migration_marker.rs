//! The markers a migration file may carry.

/// Whether the SQL's first line is the `-- no-transaction` marker: the
/// migration runs outside a transaction.
///
/// The marker is the marker, not a word: a file edited on Windows may
/// start with a BOM, and the marker may carry a note after it. The rule
/// is the same on both sides of the compiler — the runner and the
/// analyzer that replays the migrations.
pub fn has_no_transaction_marker(sql: &str) -> bool {
    let Some(first) = sql.lines().next() else {
        return false;
    };
    // A file edited on Windows starts with a BOM.
    let first = first.trim_start_matches('\u{feff}').trim();
    match first.strip_prefix("-- no-transaction") {
        Some(rest) => match rest.chars().next() {
            // The marker alone, or with a note after it.
            None => true,
            Some(c) => !c.is_alphanumeric() && c != '_',
        },
        None => false,
    }
}

/// The SQL without the byte-order mark a file edited on Windows starts
/// with: PostgreSQL rejects it as a syntax error.
pub fn strip_bom(sql: &str) -> &str {
    sql.strip_prefix('\u{feff}').unwrap_or(sql)
}

#[cfg(test)]
mod tests {
    use super::{has_no_transaction_marker, strip_bom};

    #[test]
    fn the_marker_is_read_tolerantly() {
        assert!(has_no_transaction_marker("-- no-transaction\nSELECT 1;"));
        assert!(has_no_transaction_marker(
            "\u{feff}-- no-transaction\nSELECT 1;"
        ));
        assert!(has_no_transaction_marker(
            "-- no-transaction (for CONCURRENTLY)\nSELECT 1;"
        ));
        assert!(has_no_transaction_marker(
            "  -- no-transaction  \nSELECT 1;"
        ));
        // Not the marker.
        assert!(!has_no_transaction_marker("SELECT 1;"));
        assert!(!has_no_transaction_marker("-- no-transactions are bad\n"));
        assert!(!has_no_transaction_marker("-- no_transaction\n"));
        assert!(!has_no_transaction_marker("--no-transaction\n"));
    }

    #[test]
    fn the_bom_is_stripped() {
        assert_eq!(strip_bom("\u{feff}SELECT 1;"), "SELECT 1;");
        assert_eq!(strip_bom("SELECT 1;"), "SELECT 1;");
    }
}
