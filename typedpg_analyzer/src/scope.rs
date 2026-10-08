//! Scope tracking for table aliases, columns, and CTEs.

use crate::error::{AnalyzeError, RawError, SourceSpan};
use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgAttribute, PgCatalog, SYSTEM_COLUMNS};
use crate::qualified_name::QualifiedName;
use crate::suggest::suggest_similar;

fn system_columns_for(alias: &str) -> Vec<ScopeColumn> {
    SYSTEM_COLUMNS
        .iter()
        .map(|&(name, type_oid, _attnum)| ScopeColumn {
            name: name.to_owned(),
            type_oid,
            base_not_null: true,
            typmod: None,
            collation: None,
            table_alias: alias.to_owned(),
            record_fields: None,
            elem_nullable: None,
            origin: None,
        })
        .collect()
}

/// A resolved column with its type and base nullability (from table definition).
#[derive(Debug, Clone)]
pub(crate) struct ScopeColumn {
    pub name: String,
    pub type_oid: PgTypeOid,
    /// NOT NULL from the table definition (before JOIN effects).
    pub base_not_null: bool,
    /// `pg_attribute.atttypmod`-shaped modifier (`varchar(n)` length, etc.),
    /// optionally inherited from the column's type chain (e.g. a domain over
    /// `varchar(20)`). `None` matches PG's `-1`.
    pub typmod: Option<i32>,
    /// `pg_attribute.attcollation` of the source column, if any. Threaded
    /// through `infer_column_ref` into `ExprType.collation`.
    pub collation: Option<crate::oid::PgCollationOid>,
    /// The alias of the table this column belongs to.
    pub table_alias: String,
    /// Named-field structure when the column holds a record value: SRF /
    /// OUT-arg functions populate this from `out_args`, ROW constructors fill
    /// it from the inferred shape, subqueries propagate it through.
    pub record_fields: Option<crate::expr::RecordShape>,
    /// For an array column, whether its elements can be NULL, where the
    /// query producing it knows (see [`crate::types::Type::Array`]).
    pub elem_nullable: Option<bool>,
    /// The base-table column this column's value is read from, when it
    /// is one passed through unchanged (see [`Origin`]).
    pub origin: Option<Origin>,
}

/// Where a value comes from: column `column` of base table `relid`, read
/// from one scan of it (`scan`, unique to the FROM entry that scans it —
/// every column with the same `scan` holds, in a given row, the values of
/// one and the same table row, or all NULL when the scan's row is
/// null-extended). Only a column passed through as is (a plain column
/// reference, `*`) keeps its origin: no expression, set operation or
/// grouping set that may NULL it on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Origin {
    pub scan: u32,
    pub relid: crate::oid::PgClassOid,
    pub column: String,
    /// The column's own NOT NULL in the table (and in each child a scan
    /// of an inheritance parent reads).
    pub base_not_null: bool,
    /// The scan also returns inheritance children's rows (which the
    /// table's foreign keys don't bind).
    pub with_children: bool,
    /// Every row of the table is there: the scan reads all of them (no
    /// TABLESAMPLE, no `ONLY` over a partitioned table) and every
    /// derived relation in between keeps them all (no WHERE, no
    /// grouping, no LIMIT, ...).
    pub all_rows: bool,
}

/// A fresh scan identifier (see [`Origin::scan`]).
pub(crate) fn fresh_scan_id() -> u32 {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A table-like source in the FROM clause.
#[derive(Debug, Clone)]
pub(crate) struct TableSource {
    pub alias: String,
    pub columns: Vec<ScopeColumn>,
    /// PG's hidden columns (`tableoid`, `xmin`, ...) for real relations.
    /// Kept separate so `SELECT *` expansion and ambiguity checks ignore them,
    /// while explicit `t.tableoid` references still resolve. Empty for CTEs
    /// and subqueries (PG doesn't expose system columns through those).
    pub system_columns: Vec<ScopeColumn>,
    /// Qualified name of the backing relation, or `None` for derived sources
    /// (CTE, subquery). Set for real tables/views so that `alias.*` in an
    /// expression context can look up the composite type of the relation.
    pub source_qn: Option<QualifiedName>,
    /// Column names merged away by `JOIN USING` / `NATURAL JOIN`: hidden
    /// from *unqualified* name resolution and from the bare `*` (the merged
    /// column — a synthetic source — takes their place), but still reachable
    /// qualified (`a.id`) and via `a.*`, exactly like PG. Kept on the source
    /// so the rule travels with it into sublinks and LATERAL subqueries.
    pub join_hidden: std::collections::HashSet<String>,
    /// A left-side entry of a RIGHT / FULL join seen from its LATERAL right
    /// side: PG keeps it in the namespace but rejects any reference to it
    /// (`check_lateral_ref_ok`, 42P10).
    pub lateral_blocked: bool,
    /// The relation behind a FROM item that names one (dependencies on its
    /// columns are noted as they are resolved).
    pub relid: Option<crate::oid::PgClassOid>,
    /// What kind of range-table entry PG would build for this FROM item —
    /// drives the locking-clause rules (`transformLockingClause`).
    pub kind: SourceKind,
    /// A table-only namespace item (`addNSItemToQuery` with
    /// `addToVarNameSpace = false`): reachable by its name (`new.col`,
    /// `new.*`, a whole-row `new`) but never by an unqualified column name
    /// or the bare `*`. Rule OLD / NEW and RETURNING's OLD / NEW rows.
    pub table_only: bool,
    /// The whole row may be absent (NULL): RETURNING's `old` for an INSERT,
    /// `new` for a DELETE. Its columns are then nullable too.
    pub null_row: bool,
    /// The target relation of an UPDATE / DELETE / INSERT … ON CONFLICT
    /// (PG's `p_target_nsitem`): when it is [`Self::lateral_blocked`], PG
    /// hints that the entry can't be referenced from this part of the
    /// query instead of blaming the join type.
    pub dml_target: bool,
    /// What a whole-row reference to this entry (`f`, `f.*` in an
    /// expression) yields when it has no backing relation — PG's
    /// `makeWholeRowVar` for a function RTE.
    pub whole_row: WholeRow,
    /// For a view or FROM subquery: what a locking clause pushed into it
    /// hits in the levels below it (a view's own query, an outer join's
    /// nullable side there) — the rewriter / planner errors PG raises when
    /// the statement is planned.
    pub lock_error: Option<LockBlock>,
    /// A relation scanned so that some of its rows may be missing:
    /// `TABLESAMPLE`, or `ONLY` over a partitioned table (whose rows are
    /// all in its partitions). A foreign key referencing it doesn't
    /// guarantee its referenced row shows up.
    pub partial_scan: bool,
    /// A scan of a table with (non-partition) inheritance children that
    /// also returns their rows: constraints the table doesn't pass down —
    /// foreign keys, `NO INHERIT` ones — don't bind those.
    pub inherits_rows: bool,
    /// The entry yields at least one row (for each row of the entries a
    /// LATERAL one reads): a FROM-less query, an aggregate without GROUP
    /// BY, a lookup a foreign key guarantees, ...
    pub min_one_row: bool,
}

/// Why a locking clause can't be pushed into a view or subquery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockBlock {
    /// `CheckSelectLocking`: `FOR UPDATE is not allowed with <construct>`.
    NotAllowedWith(&'static str),
    /// `make_outerjoininfo`: a marked relation on an outer join's nullable
    /// side.
    NullableSide,
}

/// The value of a whole-row reference to a FROM item without a backing
/// relation (`makeWholeRowVar`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WholeRow {
    /// An anonymous `record` of the entry's columns (subqueries, CTEs,
    /// VALUES, joins, several functions, `WITH ORDINALITY`, …).
    #[default]
    Record,
    /// A single function returning a named composite type: that type.
    Composite(PgTypeOid),
    /// A single function returning a scalar: the function's value itself.
    Scalar,
}

/// The RTE kind behind a [`TableSource`].
#[derive(Debug, Clone, Default)]
pub(crate) enum SourceKind {
    /// A table or view (`RTE_RELATION`).
    Relation,
    /// A FROM subquery (`RTE_SUBQUERY`). `lock_blocker` is the first
    /// `CheckSelectLocking` violation a locking clause pushed into it would
    /// hit (`"DISTINCT clause"`, …).
    Subquery { lock_blocker: Option<&'static str> },
    /// A CTE reference (`RTE_CTE`).
    Cte,
    /// A FROM function (`RTE_FUNCTION`).
    Function,
    /// A JOIN's merged USING columns (`RTE_JOIN`).
    Join,
    /// Anything else (VALUES lists, the INSERT `excluded` pseudo-relation).
    #[default]
    Other,
}

/// First character of the synthetic aliases given to FROM items PG exposes
/// without a referencable name (an unaliased subquery, the merged columns of
/// an unaliased `JOIN USING`). No SQL identifier can spell it.
const HIDDEN_ALIAS_MARK: char = '\u{1}';

/// A fresh alias that no reference can name. Unique process-wide, so hidden
/// sources never collide across scope tiers.
pub(crate) fn hidden_alias(kind: &str) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{HIDDEN_ALIAS_MARK}{kind}{n}")
}

/// True for the hidden alias of an unaliased `JOIN USING`'s merged columns.
pub(crate) fn is_hidden_join_alias(alias: &str) -> bool {
    alias
        .strip_prefix(HIDDEN_ALIAS_MARK)
        .is_some_and(|rest| rest.starts_with("join"))
}

/// True for an alias produced by [`hidden_alias`].
pub(crate) fn is_hidden_alias(alias: &str) -> bool {
    alias.starts_with(HIDDEN_ALIAS_MARK)
}

impl TableSource {
    /// A source that holds only `columns` (a CTE, subquery, function, or
    /// merged USING columns).
    pub(crate) fn derived(alias: &str, columns: Vec<ScopeColumn>) -> Self {
        TableSource {
            alias: alias.to_owned(),
            columns,
            system_columns: Vec::new(),
            source_qn: None,
            relid: None,
            join_hidden: Default::default(),
            lateral_blocked: false,
            kind: SourceKind::Other,
            table_only: false,
            null_row: false,
            dml_target: false,
            whole_row: WholeRow::Record,
            lock_error: None,
            partial_scan: false,
            inherits_rows: false,
            min_one_row: false,
        }
    }

    /// A reference resolved to column `name` of this FROM item.
    pub(crate) fn note_column(&self, name: &str) {
        if let Some(relid) = self.relid {
            crate::ddl::depend::note_column(relid, name);
        }
    }

    /// A `*` over this FROM item refers to each of its columns.
    pub(crate) fn note_star_columns(&self) {
        for c in &self.columns {
            self.note_column(&c.name);
        }
    }

    /// The columns an unqualified reference or the bare `*` can see.
    pub(crate) fn visible_columns(&self) -> impl Iterator<Item = &ScopeColumn> {
        self.columns
            .iter()
            .filter(|c| !self.table_only && !self.join_hidden.contains(&c.name))
    }

    /// The system columns an unqualified reference can see.
    fn visible_system_columns(&self) -> impl Iterator<Item = &ScopeColumn> {
        self.system_columns.iter().filter(|_| !self.table_only)
    }

    /// This entry under another name, as a table-only namespace item —
    /// PG's `addNSItemForReturning` copy of the target relation. With
    /// `null_row`, the row (and so every column) may be NULL.
    pub(crate) fn table_only_copy(&self, alias: &str, null_row: bool) -> Self {
        let rename = |cols: &[ScopeColumn]| -> Vec<ScopeColumn> {
            cols.iter()
                .map(|c| ScopeColumn {
                    table_alias: alias.to_owned(),
                    base_not_null: c.base_not_null && !null_row,
                    ..c.clone()
                })
                .collect()
        };
        TableSource {
            alias: alias.to_owned(),
            columns: rename(&self.columns),
            system_columns: rename(&self.system_columns),
            join_hidden: Default::default(),
            lateral_blocked: false,
            table_only: true,
            null_row,
            ..self.clone()
        }
    }
}

/// PG's `check_lateral_ref_ok` (42P10): a reference to a RIGHT / FULL
/// join's left side from its LATERAL right side, or to an UPDATE / DELETE
/// target from its FROM / USING list.
fn lateral_blocked_error(source: &TableSource, span: Option<SourceSpan>) -> AnalyzeError {
    let alias = &source.alias;
    RawError::new(
        AnalyzeError::InvalidColumnReference(format!(
            "invalid reference to FROM-clause entry for table \"{alias}\""
        )),
        span,
        Some(if source.dml_target {
            format!(
                "There is an entry for table \"{alias}\", but it cannot be referenced from this \
                 part of the query."
            )
        } else {
            "The combining JOIN type must be INNER or LEFT for a LATERAL reference.".into()
        }),
    )
    .finalize_implicit()
}

/// PG (42702): `column reference "id" is ambiguous`, listing where the name
/// was found.
fn ambiguous_column_error(
    column: &str,
    candidates: &[String],
    span: Option<SourceSpan>,
) -> AnalyzeError {
    RawError::new(
        AnalyzeError::AmbiguousColumn(format!(
            "column reference \"{column}\" is ambiguous (could be: {})",
            candidates.join(", ")
        )),
        span,
        None,
    )
    .with_primary_label("ambiguous reference")
    .finalize_implicit()
}

#[derive(Debug, Clone)]
pub(crate) struct Scope {
    pub sources: Vec<TableSource>,
    /// Same-level FROM items made visible by `LATERAL`. PG resolves them
    /// like outer references: the subquery's own `sources` win first, these
    /// come next, and ambiguity is only raised *within* a tier (two lateral
    /// sources sharing a column name is ambiguous; a lateral source sharing
    /// a name with an inner source is not). Excluded from `*` expansion —
    /// `SELECT * FROM b` inside `LATERAL (…)` produces only `b`'s columns.
    pub lateral_sources: Vec<TableSource>,
    pub outer_sources: Vec<TableSource>,
    /// Aliases that exist in the enclosing scope but are *not* visible here
    /// — same shape PG uses for non-LATERAL subqueries: a reference like
    /// `t.col` against a `t` in this list produces the diagnostic `invalid
    /// reference to FROM-clause entry for table "t"` instead of the generic
    /// `column "t.col" does not exist`. Never consulted for resolution.
    pub shadowed_sources: Vec<TableSource>,
    /// Aliases of the left side of the RIGHT / FULL join whose right side is
    /// being processed: a LATERAL item there sees them only as
    /// [`TableSource::lateral_blocked`] entries.
    pub lateral_blocked_aliases: std::collections::HashSet<String>,
    /// CTEs visible at this query level (its own WITH plus the enclosing
    /// ones). Sublinks and nested subqueries inherit them — PG resolves a
    /// CTE name by walking up the parse-state chain (`scanNameSpaceForCTE`).
    pub ctes: std::collections::HashMap<String, Vec<ScopeColumn>>,
}

thread_local! {
    /// Relations every scope sees as outer references while a rule action
    /// is analyzed — see [`with_rule_pseudo_relations`].
    static RULE_PSEUDO_RELATIONS: std::cell::RefCell<Vec<TableSource>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Run `f` with the rule's OLD / NEW pseudo-relations in the range table of
/// whatever it analyzes, as `transformRuleStmt` adds them: in the relation
/// namespace only (`addToRelNameSpace`, not `addToVarNameSpace`), so
/// `new.col`, `new.*` and a whole-row `new` resolve, while an unqualified
/// column name never reaches them. Subqueries see them as outer references.
pub(crate) fn with_rule_pseudo_relations<R>(
    snapshot: &PgCatalog,
    relation: QualifiedName,
    columns: &[PgAttribute],
    names: &[&str],
    f: impl FnOnce() -> R,
) -> R {
    let mut holder = Scope::default();
    for name in names {
        holder.add_dml_target(snapshot, name, relation.clone(), columns);
    }
    let sources = holder
        .sources
        .into_iter()
        .map(|mut s| {
            s.table_only = true;
            s
        })
        .collect();
    let prev = RULE_PSEUDO_RELATIONS.with(|r| r.replace(sources));
    let out = f();
    RULE_PSEUDO_RELATIONS.with(|r| *r.borrow_mut() = prev);
    out
}

/// parserOpenTable → table_open (validate_relation_kind): an index or a
/// composite type has no rows to read or write.
pub(crate) fn check_relation_opens(class: &crate::pg_catalog::PgClass) -> Result<(), AnalyzeError> {
    match class.relkind {
        crate::pg_catalog::RelKind::Index
        | crate::pg_catalog::RelKind::PartitionedIndex
        | crate::pg_catalog::RelKind::CompositeType => Err(crate::pgmsg::cannot_open_relation(
            &class.relname,
            class.relkind,
        )
        .finalize_implicit()),
        _ => Ok(()),
    }
}

impl Default for Scope {
    fn default() -> Self {
        Scope {
            sources: Vec::new(),
            lateral_sources: Vec::new(),
            outer_sources: RULE_PSEUDO_RELATIONS.with(|r| r.borrow().clone()),
            shadowed_sources: Vec::new(),
            lateral_blocked_aliases: Default::default(),
            ctes: Default::default(),
        }
    }
}

/// Build the public-facing `UndefinedTable` error for a missing relation,
/// picking up the snippet location from TLS and computing a "did you mean"
/// hint against the catalog's visible relations.
///
/// Used by `Scope::add_table` and by the DML statement handlers in
/// `resolve.rs` (INSERT/UPDATE/DELETE/MERGE) so the error rendering is
/// consistent across all sites.
pub(crate) fn undefined_table_error(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    span: Option<SourceSpan>,
) -> AnalyzeError {
    let hint = suggest_similar(name, snapshot.visible_relnames(schema))
        .map(|c| format!("did you mean \"{c}\"?"));
    // Match PG's wording: when the user qualified the relation, the
    // schema prefix shows up in the error too. `QualifiedName::Display`
    // handles identifier quoting for us.
    let qualified = match schema {
        Some(s) => QualifiedName::new(s, name).to_string(),
        None => name.to_string(),
    };
    RawError::undefined_table(&qualified, span, hint).finalize_implicit()
}

/// Build an `UndefinedColumn` error for a DML target column (INSERT col
/// list, UPDATE SET col). Uses the table's attributes for the suggestion
/// rather than a `Scope` (no scope exists yet during DML target validation).
pub(crate) fn undefined_dml_column_error(
    column: &str,
    table_relname: &str,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    span: Option<SourceSpan>,
) -> AnalyzeError {
    let hint = suggest_similar(column, table_attrs.iter().map(|a| a.attname.as_str()))
        .map(|c| format!("did you mean \"{c}\"?"));
    RawError::undefined_column(
        format!("column \"{column}\" of relation \"{table_relname}\" does not exist"),
        span,
        hint,
    )
    .finalize_implicit()
}

/// Build the public-facing `UndefinedColumn` error.
///
/// `message` is the PG-verbatim first line — callers pick the wording
/// matching PG's behavior (bare or qualified column). `qualifier` is the
/// table the user named, if any; the hint is [`Scope::column_hint`]'s.
fn undefined_column_error(
    scope: &Scope,
    qualifier: Option<&str>,
    column: &str,
    message: String,
    span: Option<SourceSpan>,
) -> AnalyzeError {
    let hint = scope.column_hint(qualifier, column);
    RawError::undefined_column(message, span, hint).finalize_implicit()
}

/// PG's hint naming a column (errorMissingColumn): `Perhaps you meant to
/// reference the column "t.c".`, or `… "a.c" or the column "b.c".` for
/// two equally good matches. PG builds it from the raw names
/// (`"%s.%s"`), unquoted — so do we.
fn perhaps_column_hint(matches: &[(&str, &str)]) -> Option<String> {
    let name = |(alias, column): &(&str, &str)| {
        if is_hidden_alias(alias) {
            (*column).to_owned()
        } else {
            format!("{alias}.{column}")
        }
    };
    match matches {
        [one] => Some(format!(
            "Perhaps you meant to reference the column \"{}\".",
            name(one)
        )),
        [a, b] => Some(format!(
            "Perhaps you meant to reference the column \"{}\" or the column \"{}\".",
            name(a),
            name(b)
        )),
        _ => None,
    }
}

impl Scope {
    /// PG (SQLSTATE 42712): every FROM item of one query level needs a
    /// distinct alias — `FROM users u, posts u` is rejected. The synthetic
    /// empty-alias sources produced by JOIN USING merging are exempt.
    fn check_duplicate_alias(
        &self,
        alias: &str,
        span: Option<SourceSpan>,
    ) -> Result<(), AnalyzeError> {
        if self.sources.iter().any(|s| s.alias == alias) {
            return Err(crate::pgmsg::duplicate_table_alias(alias, span).finalize_implicit());
        }
        Ok(())
    }

    /// Add a table from the catalog.
    ///
    /// Only relations with rows open (see [`check_relation_opens`]).
    ///
    /// `span` covers the relation reference in the original SQL — usually
    /// produced by `SourceSpan::from_node_qname(RangeVar.location)`. Pass
    /// `None` when no AST location is available; the resulting
    /// `UndefinedTable` error then carries no snippet (but `did you mean`
    /// still works).
    pub fn add_table(
        &mut self,
        snapshot: &PgCatalog,
        schema: Option<&str>,
        name: &str,
        alias: &str,
        span: Option<SourceSpan>,
    ) -> Result<(), AnalyzeError> {
        self.check_duplicate_alias(alias, span)?;
        let table = snapshot
            .resolve_table(schema, name)
            .ok_or_else(|| undefined_table_error(snapshot, schema, name, span))?;
        check_relation_opens(table)?;
        let table_oid = table.oid;
        crate::ddl::depend::note(crate::ddl::depend::ObjectAddress::relation(table_oid));
        let nspname = snapshot
            .namespace_name(table.relnamespace)
            .map(str::to_owned)
            .unwrap_or_else(|| "public".to_owned());
        let relname = table.relname.clone();
        // A view has no system attributes.
        let is_view = table.relkind == crate::pg_catalog::RelKind::View;
        let null_free = crate::nonnull::checks::null_free_array_columns(snapshot, table_oid);

        let columns: Vec<ScopeColumn> = snapshot
            .attributes_of(table_oid)
            .iter()
            .map(|c| ScopeColumn {
                name: c.attname.clone(),
                type_oid: c.atttypid,
                // Only the column's own NOT NULL (or a generation
                // expression that can't be NULL): a NOT NULL domain doesn't
                // keep NULL out of a column (CREATE DOMAIN's notes: a value
                // already of the domain type — an empty scalar subquery, an
                // outer join's NULL — is stored unchecked).
                base_not_null: snapshot.attr_never_null(c),
                typmod: snapshot.effective_typmod(c.atttypid, c.atttypmod),
                collation: c.attcollation,
                table_alias: alias.to_owned(),
                record_fields: None,
                elem_nullable: (null_free.contains(&c.attname)
                    || snapshot.domain_null_free_elements(c.atttypid))
                .then_some(false),
                origin: None,
            })
            .collect();

        self.sources.push(TableSource {
            system_columns: if is_view {
                Vec::new()
            } else {
                system_columns_for(alias)
            },
            source_qn: Some(QualifiedName::new(nspname, relname)),
            relid: Some(table_oid),
            kind: SourceKind::Relation,
            ..TableSource::derived(alias, columns)
        });
        Ok(())
    }

    /// Add a derived source (CTE, subquery, function result) of the given
    /// RTE kind.
    pub fn add_derived(
        &mut self,
        alias: &str,
        columns: Vec<ScopeColumn>,
        kind: SourceKind,
    ) -> Result<(), AnalyzeError> {
        self.check_duplicate_alias(alias, None)?;
        self.sources.push(TableSource {
            kind,
            ..TableSource::derived(alias, columns)
        });
        Ok(())
    }

    /// Add columns from a DML target table (for RETURNING).
    pub fn add_dml_target(
        &mut self,
        snapshot: &PgCatalog,
        alias: &str,
        qn: QualifiedName,
        columns: &[PgAttribute],
    ) {
        // The rows of an inheritance parent's children are the statement's
        // too: a column is NOT NULL only if it is in each of them.
        let mut descendants = Vec::new();
        let mut todo: Vec<crate::oid::PgClassOid> =
            columns.first().map(|c| c.attrelid).into_iter().collect();
        while let Some(r) = todo.pop() {
            for i in snapshot.pg_inherits.iter().filter(|i| i.inhparent == r) {
                if !descendants.contains(&i.inhrelid) {
                    descendants.push(i.inhrelid);
                    todo.push(i.inhrelid);
                }
            }
        }
        let not_null_everywhere = |c: &PgAttribute| {
            descendants.iter().all(|&d| {
                snapshot
                    .attributes_of(d)
                    .iter()
                    .find(|a| a.attname == c.attname)
                    .is_some_and(|a| snapshot.attr_never_null(a))
            })
        };
        let null_free = columns
            .first()
            .map(|c| crate::nonnull::checks::null_free_array_columns(snapshot, c.attrelid))
            .unwrap_or_default();
        let cols = columns
            .iter()
            .map(|c| ScopeColumn {
                name: c.attname.clone(),
                type_oid: c.atttypid,
                // A NOT NULL domain doesn't keep NULL out (see `add_table`).
                base_not_null: snapshot.attr_never_null(c) && not_null_everywhere(c),
                typmod: snapshot.effective_typmod(c.atttypid, c.atttypmod),
                collation: c.attcollation,
                table_alias: alias.to_owned(),
                record_fields: None,
                elem_nullable: (null_free.contains(&c.attname)
                    || snapshot.domain_null_free_elements(c.atttypid))
                .then_some(false),
                origin: None,
            })
            .collect();
        let relid = columns.first().map(|c| c.attrelid);
        if let Some(relid) = relid {
            crate::ddl::depend::note(crate::ddl::depend::ObjectAddress::relation(relid));
        }
        self.sources.push(TableSource {
            system_columns: system_columns_for(alias),
            source_qn: Some(qn),
            relid,
            kind: SourceKind::Relation,
            dml_target: true,
            ..TableSource::derived(alias, cols)
        });
    }

    /// Every source this level can resolve against: its own FROM items, the
    /// LATERAL tier, then the correlated outer tier.
    fn all_tiers(&self) -> impl Iterator<Item = &TableSource> {
        self.sources
            .iter()
            .chain(self.lateral_sources.iter())
            .chain(self.outer_sources.iter())
    }

    /// The sources a LATERAL item (or a FROM function's arguments) at this
    /// point may reference: this level's FROM items so far plus the lateral
    /// tier it received, with a RIGHT / FULL join's left side marked
    /// [`TableSource::lateral_blocked`].
    pub fn lateral_visible(&self) -> Vec<TableSource> {
        self.sources
            .iter()
            .chain(self.lateral_sources.iter())
            .cloned()
            .map(|mut s| {
                if self.lateral_blocked_aliases.contains(&s.alias) {
                    s.lateral_blocked = true;
                }
                s
            })
            .collect()
    }

    /// The enclosing query levels' entries this level reaches as outer
    /// references, minus the rule OLD / NEW pseudo-relations every
    /// [`Scope::default`] already starts with — what a nested subquery
    /// hands down as its correlated tier.
    pub fn enclosing_sources(&self) -> Vec<TableSource> {
        let pseudo = RULE_PSEUDO_RELATIONS.with(|r| r.borrow().len());
        self.outer_sources.iter().skip(pseudo).cloned().collect()
    }

    /// The column a plain column reference (`c`, `t.c`, `s.t.c`; not `*`)
    /// resolves to, if it resolves.
    pub(crate) fn plain_column_ref(
        &self,
        n: &typedpg_pg_query::protobuf::Node,
    ) -> Option<&ScopeColumn> {
        use typedpg_pg_query::protobuf::node;
        let Some(node::Node::ColumnRef(c)) = n.node.as_ref() else {
            return None;
        };
        if c.fields
            .iter()
            .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
        {
            return None;
        }
        let parts = crate::expr::extract_string_fields(&c.fields);
        let (table, column) = match parts.as_slice() {
            [col] => (None, col.as_str()),
            [tbl, col] => (Some(tbl.as_str()), col.as_str()),
            [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
            _ => return None,
        };
        self.resolve_column(table, column, None).ok()
    }

    pub fn find_source(&self, alias: &str) -> Option<&TableSource> {
        self.sources
            .iter()
            .chain(self.lateral_sources.iter())
            .chain(self.outer_sources.iter())
            .find(|s| s.alias == alias)
    }

    pub fn resolve_column(
        &self,
        table: Option<&str>,
        column: &str,
        span: Option<SourceSpan>,
    ) -> Result<&ScopeColumn, AnalyzeError> {
        if let Some(t) = table {
            // PG's `refnameNamespaceItem` stops at the nearest level whose
            // namespace has an entry of that name: an inner `u` hides an
            // outer one even when only the outer has the column.
            let nearest = [&self.sources, &self.lateral_sources, &self.outer_sources]
                .into_iter()
                .find(|tier| tier.iter().any(|s| s.alias == t))
                .map_or(&[][..], |tier| tier.as_slice());
            for source in nearest {
                if source.alias != t {
                    continue;
                }
                // PG checks the entry's LATERAL visibility before looking
                // the column up (`check_lateral_ref_ok`).
                if source.lateral_blocked {
                    return Err(lateral_blocked_error(source, span));
                }
                // A name the entry exposes twice (an aliased join over two
                // `id`s, a subquery `SELECT 1 a, 2 a`) is ambiguous even
                // when qualified — PG's `scanRTEForColumn`.
                let mut found = source
                    .columns
                    .iter()
                    .chain(source.system_columns.iter())
                    .filter(|c| c.name == column);
                if let Some(col) = found.next() {
                    if found.next().is_some() {
                        let qn = QualifiedName::new(t, column).to_string();
                        return Err(ambiguous_column_error(column, &[qn.clone(), qn], span));
                    }
                    source.note_column(column);
                    return Ok(col);
                }
            }
            // The alias exists but lacks the column → PG's qualified
            // missing-column wording, formatted through identifier quoting
            // rules (`column t.col does not exist`, `column "T".col does
            // not exist` when `T` needs quoting).
            let alias_exists = self.all_tiers().any(|s| s.alias == t);
            if alias_exists {
                return Err(undefined_column_error(
                    self,
                    Some(t),
                    column,
                    format!("column {} does not exist", QualifiedName::new(t, column)),
                    span,
                ));
            }
            // Match PG's wording for the non-LATERAL outer-reference case:
            // when `t` is visible in the enclosing FROM but not here, point
            // at the FROM-clause-entry visibility rule rather than the
            // generic missing-column message. The hint mirrors PG's HINT.
            // The same wording covers qualifying by a table's *real* name
            // when the FROM entry gave it an alias (`SELECT users.id FROM
            // users u` — PG hints at the alias).
            let aliased_as = self
                .all_tiers()
                .chain(self.shadowed_sources.iter())
                .find(|s| s.alias != t && s.source_qn.as_ref().is_some_and(|qn| qn.name == t))
                .map(|s| s.alias.clone());
            let shadowed = self.shadowed_sources.iter().any(|s| s.alias == t);
            if aliased_as.is_some() || shadowed {
                // PG classifies this as undefined_table (42P01): the entry
                // exists but is not referencable from here. Its hint
                // (errorMissingRTE) names the alias a relation was given;
                // otherwise its detail says why the entry is out of reach.
                let (hint, note) = match aliased_as.filter(|a| !is_hidden_alias(a)) {
                    Some(alias) => (
                        Some(format!(
                            "Perhaps you meant to reference the table alias \"{alias}\"."
                        )),
                        None,
                    ),
                    None => (
                        None,
                        Some(format!(
                            "There is an entry for table \"{t}\", but it cannot be referenced \
                             from this part of the query."
                        )),
                    ),
                };
                let mut err = crate::error::RawError::new(
                    AnalyzeError::UndefinedTable(format!(
                        "invalid reference to FROM-clause entry for table \"{t}\""
                    )),
                    span,
                    hint,
                )
                .with_primary_label("not referencable here");
                if let Some(note) = note {
                    err = err.with_note(note);
                }
                return Err(err.finalize_implicit());
            }
            // The alias matches nothing in scope at all — PG reports the
            // missing FROM entry (42P01), not a missing column.
            return Err(crate::error::RawError::new(
                AnalyzeError::UndefinedTable(format!(
                    "missing FROM-clause entry for table \"{t}\""
                )),
                span,
                self.missing_entry_hint(t),
            )
            .with_primary_label("no FROM-clause entry by this name")
            .finalize_implicit());
        }

        for tier in [&self.sources, &self.lateral_sources, &self.outer_sources] {
            let mut matches: Vec<(&TableSource, &ScopeColumn)> = Vec::new();
            for source in tier {
                // Columns merged away by JOIN USING / NATURAL are only
                // reachable qualified; the synthetic merged column stands in
                // for unqualified references.
                // Every match counts: one entry exposing the name twice is
                // as ambiguous as two entries exposing it once.
                // scanRTEForColumn also finds the relation's system columns.
                for col in source
                    .visible_columns()
                    .chain(source.visible_system_columns())
                    .filter(|c| c.name == column)
                {
                    matches.push((source, col));
                }
            }
            if matches.len() > 1 {
                let candidates: Vec<String> = matches
                    .iter()
                    .map(|(s, _)| {
                        if is_hidden_alias(&s.alias) {
                            column.to_owned()
                        } else {
                            QualifiedName::new(&s.alias, column).to_string()
                        }
                    })
                    .collect();
                return Err(ambiguous_column_error(column, &candidates, span));
            }
            if let Some((source, col)) = matches.first() {
                if source.lateral_blocked {
                    return Err(lateral_blocked_error(source, span));
                }
                source.note_column(column);
                return Ok(col);
            }
        }
        Err(undefined_column_error(
            self,
            None,
            column,
            format!("column \"{column}\" does not exist"),
            span,
        ))
    }

    /// The hint for a column reference nothing in scope resolved, like
    /// PG's errorMissingColumn: for `t.col`, a close name among `t`'s
    /// columns, else the other entries that have `col` exactly; for a bare
    /// `col`, the close names across every entry, qualified by the entry
    /// (when one or two entries have the best one).
    fn column_hint(&self, qualifier: Option<&str>, column: &str) -> Option<String> {
        let entries: Vec<&TableSource> = self.all_tiers().filter(|s| !s.table_only).collect();
        fn columns_of<'a>(s: &&'a TableSource) -> Vec<(&'a str, &'a str)> {
            s.visible_columns()
                .map(|c| (s.alias.as_str(), c.name.as_str()))
                .collect()
        }
        if let Some(t) = qualifier {
            let named: Vec<(&str, &str)> = entries
                .iter()
                .filter(|s| s.alias == t)
                .flat_map(columns_of)
                .collect();
            if let Some(best) = crate::suggest::suggest_similar(column, named.iter().map(|c| c.1)) {
                return perhaps_column_hint(&[(t, best)]);
            }
            let elsewhere: Vec<(&str, &str)> = entries
                .iter()
                .filter(|s| s.alias != t)
                .flat_map(columns_of)
                .filter(|c| c.1 == column)
                .collect();
            return perhaps_column_hint(&elsewhere);
        }
        let all: Vec<(&str, &str)> = entries.iter().flat_map(columns_of).collect();
        let best = crate::suggest::suggest_similar(column, all.iter().map(|c| c.1))?;
        let mut holders: Vec<(&str, &str)> = all.into_iter().filter(|c| c.1 == best).collect();
        holders.dedup();
        perhaps_column_hint(&holders).or_else(|| Some(format!("did you mean \"{best}\"?")))
    }

    /// The hint for `missing FROM-clause entry for table "t"`: a similar
    /// entry name, else the entries this level has.
    fn missing_entry_hint(&self, t: &str) -> Option<String> {
        let names: Vec<&str> = self
            .all_tiers()
            .map(|s| s.alias.as_str())
            .filter(|a| !is_hidden_alias(a) && !a.is_empty())
            .collect();
        if let Some(best) = crate::suggest::suggest_similar(t, names.iter().copied()) {
            return Some(format!("did you mean \"{best}\"?"));
        }
        let here: Vec<String> = self
            .sources
            .iter()
            .filter(|s| !is_hidden_alias(&s.alias) && !s.alias.is_empty())
            .map(|s| match &s.source_qn {
                Some(qn) if qn.name != s.alias => format!("\"{}\" ({})", s.alias, qn.name),
                _ => format!("\"{}\"", s.alias),
            })
            .collect();
        (!here.is_empty()).then(|| format!("the FROM clause has {}", here.join(", ")))
    }

    /// Columns the bare `*` expands to: everything visible, minus the
    /// constituents merged away by JOIN USING / NATURAL (their synthetic
    /// merged column — placed before both sides — stands in for them).
    /// `t.*` deliberately does NOT use this: PG includes the join columns
    /// when the star is qualified.
    pub fn star_columns(&self) -> Vec<&ScopeColumn> {
        self.sources
            .iter()
            .flat_map(|s| {
                s.note_star_columns();
                s.visible_columns()
            })
            .collect()
    }
}
