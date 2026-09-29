//! Nullability propagation through JOINs and expressions.

use std::collections::HashSet;

/// Tracks which table aliases are on the nullable side of an outer JOIN,
/// and whether the current SELECT has a GROUP BY clause.
#[derive(Debug, Clone, Default)]
pub(crate) struct NullabilityContext {
    /// Aliases that are on the nullable side of some JOIN.
    nullable_aliases: HashSet<String>,
    /// Whether the current SELECT has a GROUP BY clause.
    /// When true, each group has ≥1 row, so aggregates with NOT NULL inputs
    /// produce NOT NULL results.
    pub has_group_by: bool,
    /// `(table_alias, column_name)` pairs for columns that are present in
    /// some grouping sets but omitted from others (under `GROUPING SETS`,
    /// `ROLLUP`, or `CUBE`). PG fills these with NULL for the rows of any
    /// grouping set that doesn't include them, so the analyzer must promote
    /// such references to nullable.
    pub grouping_omitted: HashSet<(String, String)>,
    /// Whether any grouping set in the current `GROUP BY` is empty — i.e.
    /// the query carries an "aggregate over the whole input" row. When the
    /// table is empty, that row produces NULL for non-COUNT aggregates, so
    /// they must be reported as nullable even with a `GROUP BY` present.
    /// `ROLLUP(...)` and `CUBE(...)` always include the empty set; explicit
    /// `GROUPING SETS (..., ())` does too.
    pub has_empty_grouping_set: bool,
    /// The select list holds more than one set-returning function call.
    /// PG evaluates them in lockstep (`ProjectSet`) and pads the ones that
    /// run out of rows first with NULL, so every SRF result is nullable.
    pub srfs_in_lockstep: bool,
}

impl NullabilityContext {
    /// Mark all aliases from a list as nullable (used for FULL JOIN).
    pub fn mark_all_nullable(&mut self, aliases: &[String]) {
        for a in aliases {
            self.nullable_aliases.insert(a.clone());
        }
    }

    /// Whether the source `table_alias` is on the nullable side of an outer
    /// join — a whole-row reference to it (`SELECT u FROM a LEFT JOIN u …`)
    /// is then NULL for the null-extended rows.
    pub fn alias_is_nullable(&self, table_alias: &str) -> bool {
        self.nullable_aliases.contains(table_alias)
    }

    /// Check if a column is nullable, considering the column's base
    /// definition, whether its source table is on a nullable JOIN side, and
    /// whether some grouping set in `GROUPING SETS`/`ROLLUP`/`CUBE` omits it.
    pub fn is_nullable(&self, table_alias: &str, column_name: &str, base_not_null: bool) -> bool {
        if self.nullable_aliases.contains(table_alias) {
            // Table is on nullable side of JOIN → column is always nullable.
            return true;
        }
        if self
            .grouping_omitted
            .contains(&(table_alias.to_owned(), column_name.to_owned()))
        {
            // Some grouping set excludes this column → PG produces NULL there.
            return true;
        }
        // Column nullability comes from table definition.
        !base_not_null
    }
}

/// Collect all table aliases from a scope source list.
pub(crate) fn collect_aliases(sources: &[crate::scope::TableSource]) -> Vec<String> {
    sources.iter().map(|s| s.alias.clone()).collect()
}
