//! Nullability propagation through JOINs and expressions.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use crate::nonnull::checks::{Knowledge, RelationChecks};
use crate::nonnull::{Col, Facts, Literal};

/// The kind of a join, as far as nullability goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

/// A join whose ON clause is a foreign key's equalities: every row of the
/// referencing (child) side whose key columns are non-NULL has its
/// referenced (parent) row, so the parent side is never null-extended for
/// it. The constraint is assumed to hold as declared.
#[derive(Debug, Clone)]
pub(crate) struct FkMatch {
    /// The parent is the join's left side (else its right side).
    pub parent_is_left: bool,
    /// The child's key columns, with their own NOT NULL.
    pub child_cols: Vec<(Col, bool)>,
    /// They were all non-NULL (and the child not null-extended) where the
    /// join was formed.
    pub child_present: bool,
}

/// One join of this query level's FROM clause: its kind as written, the
/// FROM entries on each side and what its ON clause proves non-NULL.
#[derive(Debug, Clone)]
struct JoinRecord {
    kind: JoinKind,
    left: HashSet<String>,
    right: HashSet<String>,
    on: Facts,
    fk: Option<FkMatch>,
}

/// A `JOIN USING` merged column: which join, and its two constituents.
#[derive(Debug, Clone)]
pub(crate) struct Merged {
    pub join: usize,
    /// The constituents, with their own NOT NULL.
    pub left: (Col, bool),
    pub right: (Col, bool),
    /// Whether each constituent was NOT NULL inside the join, where it is
    /// there (a FULL join's merged column is COALESCE of both).
    pub inside_not_null: (bool, bool),
    /// The join qual `l = r` is strict: a joined pair has both non-NULL.
    pub eq_strict: bool,
}

/// The CHECK constraints of a FROM entry and its columns' own NOT NULL.
#[derive(Debug, Clone)]
struct EntryChecks {
    alias: String,
    checks: Rc<RelationChecks>,
    base_not_null: Rc<HashMap<String, bool>>,
}

/// The levels facts hold at: whenever an entry's row is there (ON
/// clauses, CHECK constraints), for every row past WHERE, and where a
/// value is read (HAVING, a WHEN, a FILTER).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Matched,
    Where,
    Local,
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
    /// What the WHERE clause proves — before grouping, so a grouping set
    /// that omits a column still makes it NULL.
    where_facts: Facts,
    /// What holds where the value is read: HAVING (after grouping), a CASE
    /// branch's WHEN or an aggregate's FILTER.
    local_facts: Facts,
    /// Entries some fact above proves not null-extended: the outer joins
    /// with one on their nullable side are reduced (`reduce_outer_joins`).
    forced_rels: HashSet<String>,
    /// `JOIN USING` merged columns, by `(alias, column)`.
    merged: HashMap<Col, Merged>,
    /// The columns of an aliased join (`(a JOIN b) AS j`): the column of
    /// the entry inside each one is (and its own NOT NULL).
    aliased: HashMap<Col, (Col, bool)>,
    /// The CHECK constraints of this level's relations.
    checks: Vec<EntryChecks>,
    /// Derived by [`Self::recompute`]: each join's kind once reduced, the
    /// entries on the nullable side of an outer join, what ON clauses
    /// prove whenever their entry's row is there, and what the CHECK
    /// constraints and disjunctions prove at each level.
    final_kinds: Vec<JoinKind>,
    join_nullable: HashSet<String>,
    matched_facts: Facts,
    derived_matched: Facts,
    derived_where: Facts,
    derived_local: Facts,
    /// Whether the current SELECT has a GROUP BY clause.
    /// When true, each group has ≥1 row, so aggregates with NOT NULL inputs
    /// produce NOT NULL results.
    pub has_group_by: bool,
    /// The query yields its rows only from a non-empty input: a HAVING
    /// that is FALSE for an empty one (`HAVING count(*) > 0`) — so an
    /// aggregate without GROUP BY sees rows.
    pub input_not_empty: bool,
    /// Columns HAVING proves some row of each group left has non-NULL
    /// (`HAVING count(b) > 0`): an aggregate over one, NULL only when no
    /// input is non-NULL (`max(b)`), isn't NULL there.
    pub nonnull_agg_inputs: Vec<Col>,
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

    /// Record a join of this level: its sides' entries, what its ON
    /// clause proves (facts about entries outside the join are dropped)
    /// and the foreign key its ON clause follows, if any. Returns the
    /// join's index.
    pub fn record_join(
        &mut self,
        kind: JoinKind,
        left: &[String],
        right: &[String],
        on: Facts,
        fk: Option<FkMatch>,
    ) -> usize {
        let left: HashSet<String> = left.iter().cloned().collect();
        let right: HashSet<String> = right.iter().cloned().collect();
        let sides: HashSet<String> = left.union(&right).cloned().collect();
        self.joins.push(JoinRecord {
            kind,
            left,
            right,
            on: on.restricted_to(&sides),
            fk,
        });
        self.recompute();
        self.joins.len() - 1
    }

    /// Record the merged columns of join `join` (`USING` / `NATURAL`): for
    /// each, its `(alias, column)` and its left and right constituents
    /// with their own NOT NULL.
    pub fn record_merged(&mut self, columns: Vec<(Col, Merged)>) {
        self.merged.extend(columns);
    }

    /// Record that column `col` of an aliased join is column `inner` of an
    /// entry inside it.
    pub fn record_aliased(&mut self, col: Col, inner: Col, inner_base_not_null: bool) {
        // Map straight to the innermost column, so no chain (nor a cycle
        // through a reused name, `(t JOIN u) AS t`) is ever followed.
        let target = self
            .aliased
            .get(&inner)
            .cloned()
            .unwrap_or((inner, inner_base_not_null));
        if target.0 != col {
            self.aliased.insert(col, target);
        }
    }

    /// Register the CHECK constraints of relation entry `alias`, with its
    /// columns' own NOT NULL.
    pub fn register_checks(
        &mut self,
        alias: &str,
        checks: RelationChecks,
        base_not_null: HashMap<String, bool>,
    ) {
        self.checks.push(EntryChecks {
            alias: alias.to_owned(),
            checks: Rc::new(checks),
            base_not_null: Rc::new(base_not_null),
        });
        self.recompute();
    }

    /// `facts` plus the same facts about the entries inside aliased joins.
    fn translated(&self, mut facts: Facts) -> Facts {
        if self.aliased.is_empty() {
            return facts;
        }
        let inner = |c: &Col| self.aliased.get(c).map(|(i, _)| i.clone());
        let cols: Vec<Col> = facts.columns.iter().filter_map(inner).collect();
        for c in cols {
            facts.rels.insert(c.0.clone());
            facts.columns.insert(c);
        }
        let nulls: Vec<Col> = facts.nulls.iter().filter_map(inner).collect();
        facts.nulls.extend(nulls);
        let equals: Vec<(Col, Literal)> = facts
            .equals
            .iter()
            .filter_map(|(c, v)| inner(c).map(|i| (i, v.clone())))
            .collect();
        facts.equals.extend(equals);
        facts
    }

    /// Add what the WHERE clause proves non-NULL.
    pub fn add_where_facts(&mut self, facts: Facts) {
        let facts = self.translated(facts);
        self.forced_rels.extend(facts.rels.iter().cloned());
        self.where_facts = std::mem::take(&mut self.where_facts).union(facts);
        self.recompute();
    }

    /// Add what holds where the value is read (HAVING, a WHEN, a FILTER).
    pub fn add_local_facts(&mut self, facts: Facts) {
        let facts = self.translated(facts);
        self.forced_rels.extend(facts.rels.iter().cloned());
        self.local_facts = std::mem::take(&mut self.local_facts).union(facts);
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

    /// Reduce the joins, then derive what CHECK constraints and
    /// disjunctions prove; a derived column forces its entry, which may
    /// reduce more joins — until nothing changes.
    fn recompute(&mut self) {
        // From scratch: what held before a join made an entry nullable
        // may not hold anymore.
        self.derived_matched = Facts::default();
        self.derived_where = Facts::default();
        self.derived_local = Facts::default();
        for _ in 0..8 {
            self.reduce_joins();
            if !self.derive() {
                return;
            }
        }
    }

    /// PG's `reduce_outer_joins`, outermost join first: an outer join
    /// whose nullable side holds an entry proven not null-extended is an
    /// inner join (a FULL one loses that side's null-extension), and the
    /// ON clause of a join that is now inner proves its facts about both
    /// sides — passed down to the joins below, as PG passes
    /// `nonnullable_rels`. A LEFT join's ON proves its facts about its
    /// nullable side only (the preserved side's rows stay when it fails),
    /// a FULL join's about neither. A join following a foreign key never
    /// null-extends its parent side.
    fn reduce_joins(&mut self) {
        let mut forced = self.forced_rels.clone();
        forced.extend(self.derived_where.rels.iter().cloned());
        forced.extend(self.derived_local.rels.iter().cloned());
        let mut join_nullable = HashSet::new();
        let mut matched = Facts::default();
        let mut kinds = vec![JoinKind::Inner; self.joins.len()];
        for (idx, j) in self.joins.iter().enumerate().rev() {
            let mut kind = j.kind;
            if let Some(fk) = &j.fk
                && self.fk_child_present(fk, &forced)
            {
                kind = match (kind, fk.parent_is_left) {
                    (JoinKind::Left, false) | (JoinKind::Right, true) => JoinKind::Inner,
                    // The parent side of a FULL join is never
                    // null-extended: what's left is the child's.
                    (JoinKind::Full, false) => JoinKind::Right,
                    (JoinKind::Full, true) => JoinKind::Left,
                    (k, _) => k,
                };
            }
            let l = j.left.iter().any(|a| forced.contains(a));
            let r = j.right.iter().any(|a| forced.contains(a));
            let kind = match (kind, l, r) {
                (JoinKind::Left, _, true) | (JoinKind::Right, true, _) => JoinKind::Inner,
                (JoinKind::Full, true, true) => JoinKind::Inner,
                (JoinKind::Full, true, false) => JoinKind::Left,
                (JoinKind::Full, false, true) => JoinKind::Right,
                (k, _, _) => k,
            };
            kinds[idx] = kind;
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
            let side: HashSet<String> = match proven {
                Some(side) => side.clone(),
                None => j.left.union(&j.right).cloned().collect(),
            };
            let on = self
                .translated(j.on.clone())
                .restricted_to(&side_with_inner(&side, &self.aliased));
            forced.extend(on.rels.iter().cloned());
            matched = matched.union(on);
        }
        self.final_kinds = kinds;
        self.join_nullable = join_nullable;
        self.matched_facts = matched;
    }

    /// Whether a foreign-key join's child rows are there with non-NULL
    /// keys in every row the query yields: so where the join was formed,
    /// or by what WHERE (or a value's own context) proves of the keys —
    /// or of the child, forced (by a qual or a join above, never this
    /// join's own ON) not null-extended, with NOT NULL keys.
    fn fk_child_present(&self, fk: &FkMatch, forced: &HashSet<String>) -> bool {
        fk.child_present
            || fk.child_cols.iter().all(|(c, base)| {
                self.where_facts.columns.contains(c)
                    || self.local_facts.columns.contains(c)
                    || self.derived_where.columns.contains(c)
                    || self.derived_local.columns.contains(c)
                    || (*base && forced.contains(&c.0))
            })
    }

    /// What the CHECK constraints and the disjunctions prove at each
    /// level, given everything else. Returns whether anything new was
    /// found.
    fn derive(&mut self) -> bool {
        let before = (
            self.derived_matched.clone(),
            self.derived_where.clone(),
            self.derived_local.clone(),
        );
        for level in [Level::Matched, Level::Where, Level::Local] {
            let mut found = Facts::default();
            for entry in &self.checks {
                // Past the row-is-there level, a NULL-extended row breaks
                // no constraint: only an entry that is always there counts.
                if level != Level::Matched && self.alias_is_nullable(&entry.alias) {
                    continue;
                }
                let non_null = |c: &str| {
                    !self.nullable_at(
                        level,
                        &entry.alias,
                        c,
                        entry.base_not_null.get(c) == Some(&true),
                    )
                };
                let null = |c: &str| self.null_at(level, &(entry.alias.clone(), c.to_owned()));
                let equals = |c: &str| self.equals_at(level, &(entry.alias.clone(), c.to_owned()));
                let k = Knowledge {
                    non_null: &non_null,
                    null: &null,
                    equals: &equals,
                };
                found = found.union(entry.checks.derive(&entry.alias, &k));
            }
            // A disjunction with all but one of its columns NULL proves
            // that one.
            for d in self.disjunctions_at(level) {
                let left: BTreeSet<Col> = d
                    .iter()
                    .filter(|c| !self.null_at(level, c))
                    .cloned()
                    .collect();
                if left.len() == 1 {
                    let (a, c) = left.into_iter().next().expect("one column");
                    found = found.union(Facts::column(&a, &c));
                }
            }
            // Under grouping sets, a value read after grouping may be one
            // a grouping set nulled out: nothing is derived for those.
            if level == Level::Local {
                found.columns.retain(|c| !self.grouping_omitted.contains(c));
                found
                    .disjunctions
                    .retain(|d| d.iter().all(|c| !self.grouping_omitted.contains(c)));
            }
            let target = match level {
                Level::Matched => &mut self.derived_matched,
                Level::Where => &mut self.derived_where,
                Level::Local => &mut self.derived_local,
            };
            if level == Level::Matched {
                // A row-is-there fact forces nothing.
                found.rels.clear();
            }
            *target = std::mem::take(target).union(found);
        }
        before
            != (
                self.derived_matched.clone(),
                self.derived_where.clone(),
                self.derived_local.clone(),
            )
    }

    /// Whether column `(alias, column)` may be NULL at `level`.
    fn nullable_at(&self, level: Level, alias: &str, column: &str, base_not_null: bool) -> bool {
        let key = (alias.to_owned(), column.to_owned());
        if level == Level::Local
            && (self.local_facts.columns.contains(&key)
                || self.derived_local.columns.contains(&key))
        {
            return false;
        }
        if level != Level::Matched
            && (self.where_facts.columns.contains(&key)
                || self.derived_where.columns.contains(&key))
        {
            return false;
        }
        !(base_not_null
            || self.matched_facts.columns.contains(&key)
            || self.derived_matched.columns.contains(&key))
    }

    fn null_at(&self, level: Level, c: &Col) -> bool {
        self.matched_facts.nulls.contains(c)
            || self.derived_matched.nulls.contains(c)
            || (level != Level::Matched
                && (self.where_facts.nulls.contains(c) || self.derived_where.nulls.contains(c)))
            || (level == Level::Local
                && (self.local_facts.nulls.contains(c) || self.derived_local.nulls.contains(c)))
    }

    fn equals_at(&self, level: Level, c: &Col) -> Option<Literal> {
        let mut sources = vec![&self.matched_facts];
        if level != Level::Matched {
            sources.push(&self.where_facts);
        }
        if level == Level::Local {
            sources.push(&self.local_facts);
        }
        sources.into_iter().find_map(|f| f.equals.get(c).cloned())
    }

    /// The disjunctions known at `level`: a row-is-there one only while
    /// its entries are always there; before grouping ones not over a
    /// column a grouping set may null out, when read after it.
    fn disjunctions_at(&self, level: Level) -> Vec<BTreeSet<Col>> {
        let mut out: Vec<BTreeSet<Col>> = Vec::new();
        let matched = self
            .matched_facts
            .disjunctions
            .iter()
            .chain(&self.derived_matched.disjunctions);
        for d in matched {
            if level == Level::Matched || d.iter().all(|(a, _)| !self.alias_is_nullable(a)) {
                out.push(d.clone());
            }
        }
        if level != Level::Matched {
            out.extend(
                self.where_facts
                    .disjunctions
                    .iter()
                    .chain(&self.derived_where.disjunctions)
                    .cloned(),
            );
        }
        if level == Level::Local {
            out.retain(|d| d.iter().all(|c| !self.grouping_omitted.contains(c)));
            out.extend(
                self.local_facts
                    .disjunctions
                    .iter()
                    .chain(&self.derived_local.disjunctions)
                    .cloned(),
            );
        }
        out
    }

    /// Whether some grouping set leaves out a grouped expression (not a
    /// plain column): see [`crate::grouping::expr_key`].
    pub fn grouping_omits_exprs(&self) -> bool {
        !self.grouping_omitted.is_empty()
            && self
                .grouping_omitted
                .iter()
                .any(|(a, _)| a == crate::grouping::EXPR_KEY)
    }

    /// Set the columns some grouping set leaves out — with, for each, the
    /// columns that are the same value: a `JOIN USING` merged column and
    /// its constituents, an aliased join's column and the one inside.
    pub fn set_grouping_omitted(&mut self, omitted: HashSet<Col>) {
        let mut all = omitted.clone();
        for c in &omitted {
            if let Some(m) = self.merged.get(c) {
                all.insert(m.left.0.clone());
                all.insert(m.right.0.clone());
            }
            if let Some((inner, _)) = self.aliased.get(c) {
                all.insert(inner.clone());
            }
        }
        for (col, m) in &self.merged {
            if omitted.contains(&m.left.0) || omitted.contains(&m.right.0) {
                all.insert(col.clone());
            }
        }
        for (col, (inner, _)) in &self.aliased {
            if omitted.contains(inner) {
                all.insert(col.clone());
            }
        }
        self.grouping_omitted = all;
    }

    /// Whether at least one of `cols` is known non-NULL where the value is
    /// read: what COALESCE / GREATEST / LEAST over them needs to be.
    pub fn some_non_null(&self, cols: &[Col]) -> bool {
        let set: HashSet<&Col> = cols.iter().collect();
        self.disjunctions_at(Level::Local)
            .iter()
            .any(|d| d.iter().all(|c| set.contains(c)))
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
    /// outer join; for a `JOIN USING` merged column or an aliased join's,
    /// the columns it is made of; an ON clause or a CHECK constraint
    /// proving it non-NULL whenever its row is there; and the column's own
    /// definition.
    pub fn is_nullable(&self, table_alias: &str, column_name: &str, base_not_null: bool) -> bool {
        self.is_nullable_at_depth(table_alias, column_name, base_not_null, 0)
    }

    /// [`Self::is_nullable`], following merged and aliased-join columns at
    /// most a few levels deep: a reused name (`(t JOIN u USING (id)) AS t`)
    /// can make them point back at themselves, and then nothing is assumed.
    fn is_nullable_at_depth(
        &self,
        table_alias: &str,
        column_name: &str,
        base_not_null: bool,
        depth: u32,
    ) -> bool {
        if depth > 16 {
            return true;
        }
        let key = (table_alias.to_owned(), column_name.to_owned());
        let holds = |f: &Facts| !f.columns.is_empty() && f.columns.contains(&key);
        if holds(&self.local_facts) || holds(&self.derived_local) {
            return false;
        }
        if !self.grouping_omitted.is_empty() && self.grouping_omitted.contains(&key) {
            // Some grouping set excludes this column → PG produces NULL there.
            return true;
        }
        if holds(&self.where_facts) || holds(&self.derived_where) {
            return false;
        }
        if self.alias_is_nullable(table_alias) {
            // Table is on nullable side of JOIN → column is always nullable.
            return true;
        }
        if holds(&self.matched_facts) || holds(&self.derived_matched) {
            return false;
        }
        if let Some(m) = self.merged.get(&key) {
            let side =
                |(c, base): &(Col, bool)| self.is_nullable_at_depth(&c.0, &c.1, *base, depth + 1);
            let written = self.joins.get(m.join).map_or(JoinKind::Full, |j| j.kind);
            let now = self
                .final_kinds
                .get(m.join)
                .copied()
                .unwrap_or(JoinKind::Full);
            // buildMergedJoinVar: the left value for an inner or LEFT join
            // (an inner pair is equal, and both non-NULL under a strict
            // `=`), the right one for a RIGHT join, COALESCE of both for a
            // FULL one — NULL only when the side(s) its rows have are.
            return match (written, now) {
                (_, JoinKind::Inner) if m.eq_strict => false,
                (JoinKind::Inner | JoinKind::Left, _) => side(&m.left),
                (JoinKind::Right, _) => side(&m.right),
                (JoinKind::Full, JoinKind::Inner) => side(&m.left) && side(&m.right),
                (JoinKind::Full, JoinKind::Left) => side(&m.left),
                (JoinKind::Full, JoinKind::Right) => side(&m.right),
                (JoinKind::Full, JoinKind::Full) => !(m.inside_not_null.0 && m.inside_not_null.1),
            };
        }
        if let Some((inner, base)) = self.aliased.get(&key) {
            return self.is_nullable_at_depth(&inner.0, &inner.1, *base, depth + 1);
        }
        // Column nullability comes from table definition.
        !base_not_null
    }
}

/// `side` plus the entries inside its aliased joins.
fn side_with_inner(side: &HashSet<String>, aliased: &HashMap<Col, (Col, bool)>) -> HashSet<String> {
    let mut out = side.clone();
    for ((j, _), ((inner, _), _)) in aliased {
        if side.contains(j) {
            out.insert(inner.clone());
        }
    }
    out
}

/// Collect all table aliases from a scope source list.
pub(crate) fn collect_aliases(sources: &[crate::scope::TableSource]) -> Vec<String> {
    sources.iter().map(|s| s.alias.clone()).collect()
}
