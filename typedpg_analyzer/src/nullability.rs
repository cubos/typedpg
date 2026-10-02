//! Nullability propagation through JOINs and expressions.

use std::collections::HashSet;

use crate::nonnull::Facts;

/// The kind of a join, as far as nullability goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

/// One join of this query level's FROM clause: its kind as written, the
/// FROM entries on each side and what its ON clause proves non-NULL.
#[derive(Debug, Clone)]
struct JoinRecord {
    kind: JoinKind,
    left: HashSet<String>,
    right: HashSet<String>,
    on: Facts,
}

/// Tracks which table aliases are on the nullable side of an outer JOIN,
/// what the quals evaluated so far prove non-NULL, and whether the current
/// SELECT has a GROUP BY clause.
#[derive(Debug, Clone, Default)]
pub(crate) struct NullabilityContext {
    /// Aliases marked nullable outright (MERGE's RETURNING source).
    nullable_aliases: HashSet<String>,
    /// This level's joins, innermost first (each is recorded after both
    /// of its sides).
    joins: Vec<JoinRecord>,
    /// Columns the WHERE clause proves non-NULL — before grouping, so a
    /// grouping set that omits one still makes it NULL.
    where_facts: HashSet<(String, String)>,
    /// Columns proven non-NULL where the value is read: by HAVING (after
    /// grouping), a CASE branch's WHEN or an aggregate's FILTER.
    local_facts: HashSet<(String, String)>,
    /// Entries some fact above proves not null-extended: the outer joins
    /// with one on their nullable side are reduced (`reduce_outer_joins`).
    forced_rels: HashSet<String>,
    /// Derived from `joins` and `forced_rels` by [`Self::recompute`]: the
    /// entries on the nullable side of an outer join, and the columns an
    /// ON clause proves non-NULL whenever its entry's row is present.
    join_nullable: HashSet<String>,
    matched_facts: HashSet<(String, String)>,
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
    /// The frame options (`FRAMEOPTION_*` bits) of each window the SELECT's
    /// WINDOW clause names — what `OVER w` runs over.
    pub window_frames: std::collections::HashMap<String, i32>,
}

impl NullabilityContext {
    /// Mark all aliases from a list as nullable.
    pub fn mark_all_nullable(&mut self, aliases: &[String]) {
        for a in aliases {
            self.nullable_aliases.insert(a.clone());
        }
    }

    /// Record a join of this level: its sides' entries and what its ON
    /// clause proves (facts about entries outside the join are dropped).
    pub fn record_join(&mut self, kind: JoinKind, left: &[String], right: &[String], on: Facts) {
        let left: HashSet<String> = left.iter().cloned().collect();
        let right: HashSet<String> = right.iter().cloned().collect();
        let sides: HashSet<String> = left.union(&right).cloned().collect();
        self.joins.push(JoinRecord {
            kind,
            left,
            right,
            on: on.restricted_to(&sides),
        });
        self.recompute();
    }

    /// Add what the WHERE clause proves non-NULL.
    pub fn add_where_facts(&mut self, facts: Facts) {
        self.where_facts.extend(facts.columns);
        self.forced_rels.extend(facts.rels);
        self.recompute();
    }

    /// Add what holds where the value is read (HAVING, a WHEN, a FILTER).
    pub fn add_local_facts(&mut self, facts: Facts) {
        self.local_facts.extend(facts.columns);
        self.forced_rels.extend(facts.rels);
        self.recompute();
    }

    /// This context with `facts` holding too, if they say anything.
    pub fn with_local_facts(&self, facts: Facts) -> Option<NullabilityContext> {
        if facts.is_empty() {
            return None;
        }
        let mut narrowed = self.clone();
        narrowed.add_local_facts(facts);
        Some(narrowed)
    }

    /// PG's `reduce_outer_joins`, outermost join first: an outer join
    /// whose nullable side holds an entry proven not null-extended is an
    /// inner join (a FULL one loses that side's null-extension), and the
    /// ON clause of a join that is now inner proves its facts about both
    /// sides — passed down to the joins below, as PG passes
    /// `nonnullable_rels`. A LEFT join's ON proves its facts about its
    /// nullable side only (the preserved side's rows stay when it fails),
    /// a FULL join's about neither.
    fn recompute(&mut self) {
        let mut forced = self.forced_rels.clone();
        let mut join_nullable = HashSet::new();
        let mut matched = HashSet::new();
        for j in self.joins.iter().rev() {
            let l = j.left.iter().any(|a| forced.contains(a));
            let r = j.right.iter().any(|a| forced.contains(a));
            let kind = match (j.kind, l, r) {
                (JoinKind::Left, _, true) | (JoinKind::Right, true, _) => JoinKind::Inner,
                (JoinKind::Full, true, true) => JoinKind::Inner,
                (JoinKind::Full, true, false) => JoinKind::Left,
                (JoinKind::Full, false, true) => JoinKind::Right,
                (k, _, _) => k,
            };
            let proven: Option<&HashSet<String>> = match kind {
                JoinKind::Inner => None,
                JoinKind::Left => {
                    join_nullable.extend(j.right.iter().cloned());
                    Some(&j.right)
                }
                JoinKind::Right => {
                    join_nullable.extend(j.left.iter().cloned());
                    Some(&j.left)
                }
                JoinKind::Full => {
                    join_nullable.extend(j.left.iter().chain(&j.right).cloned());
                    continue;
                }
            };
            let applies = |a: &String| proven.is_none_or(|side| side.contains(a));
            forced.extend(j.on.rels.iter().filter(|a| applies(a)).cloned());
            matched.extend(j.on.columns.iter().filter(|(a, _)| applies(a)).cloned());
        }
        self.join_nullable = join_nullable;
        self.matched_facts = matched;
    }

    /// `sources` (FROM entries of this level, handed down to a nested
    /// query as outer or LATERAL references) with this level's
    /// nullability baked into their columns: the nested query has its own
    /// context, which knows nothing of this level's joins and quals.
    pub fn bake_sources(
        &self,
        sources: &[crate::scope::TableSource],
    ) -> Vec<crate::scope::TableSource> {
        sources
            .iter()
            .map(|s| {
                let mut s = s.clone();
                for c in s.columns.iter_mut().chain(s.system_columns.iter_mut()) {
                    c.base_not_null = !self.is_nullable(&c.table_alias, &c.name, c.base_not_null);
                }
                s.null_row |= self.alias_is_nullable(&s.alias);
                s
            })
            .collect()
    }

    /// The frame options a window call's `OVER` clause runs over: its own,
    /// or — for `OVER w` — those of the named window (`None` when unknown).
    pub fn window_frame_options(
        &self,
        over: &typedpg_pg_query::protobuf::WindowDef,
    ) -> Option<i32> {
        if over.name.is_empty() {
            Some(over.frame_options)
        } else {
            self.window_frames.get(&over.name).copied()
        }
    }

    /// Whether the source `table_alias` is on the nullable side of an outer
    /// join — a whole-row reference to it (`SELECT u FROM a LEFT JOIN u …`)
    /// is then NULL for the null-extended rows.
    pub fn alias_is_nullable(&self, table_alias: &str) -> bool {
        self.nullable_aliases.contains(table_alias) || self.join_nullable.contains(table_alias)
    }

    /// Check if a column is nullable, considering, in order: a qual proving
    /// it non-NULL where it is read (HAVING, WHEN, FILTER); a grouping set
    /// in `GROUPING SETS`/`ROLLUP`/`CUBE` omitting it; the WHERE clause
    /// proving it non-NULL; its source being on the nullable side of an
    /// outer join; an ON clause proving it non-NULL whenever its row is
    /// there; and the column's own definition.
    pub fn is_nullable(&self, table_alias: &str, column_name: &str, base_not_null: bool) -> bool {
        let holds = |set: &HashSet<(String, String)>| {
            !set.is_empty() && set.contains(&(table_alias.to_owned(), column_name.to_owned()))
        };
        if holds(&self.local_facts) {
            return false;
        }
        if holds(&self.grouping_omitted) {
            // Some grouping set excludes this column → PG produces NULL there.
            return true;
        }
        if holds(&self.where_facts) {
            return false;
        }
        if self.alias_is_nullable(table_alias) {
            // Table is on nullable side of JOIN → column is always nullable.
            return true;
        }
        if holds(&self.matched_facts) {
            return false;
        }
        // Column nullability comes from table definition.
        !base_not_null
    }
}

/// Collect all table aliases from a scope source list.
pub(crate) fn collect_aliases(sources: &[crate::scope::TableSource]) -> Vec<String> {
    sources.iter().map(|s| s.alias.clone()).collect()
}
