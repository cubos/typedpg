//! SQL lint rules for typedpg queries.
//!
//! [`lint_query`] analyzes a query through [`PgCatalog::query_view`] — the
//! text and parse tree as written, before the query simplifications — and
//! runs every enabled [`Rule`] over it. Findings carry the configured
//! severity, a byte range in the original query text, a message and an
//! optional help line.
//!
//! A comment `-- typedpg-lint: allow(rule_a, rule_b)` (or the `/* … */`
//! form) in the query suppresses those rules for the whole query. An
//! unknown rule name in one is an error finding that cannot be configured.
//!
//! [`lint_migrations`] applies a database's migration files and runs the
//! migration rules on tables that already had rows when each file started,
//! then the schema rules over the final catalog. Its findings are placed in
//! the file text; the TypeScript CLI prints them beside the query findings.
//!
//! [`report`] builds and renders the one report both CLIs print: the
//! `typedpg check` boxes in the human format, as one JSON object, or as
//! SARIF.

mod call_site;
mod ignore;
mod migrations;
pub mod report;
mod rules;

pub use ignore::IgnoreSet;
pub use migrations::{
    LintedMigrations, MigrationFile, MigrationFinding, MigrationLint, lint_migrations,
};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use typedpg_analyzer::{LocatedError, PgCatalog};
use typedpg_core::config::LintConfig;
use typedpg_pg_query::protobuf::Token;

/// How severe a finding is, ordered from least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    /// The severity's configuration key and rendered name.
    pub fn name(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }

    /// The severity named `name` (see [`Severity::name`]).
    pub fn from_name(name: &str) -> Option<Severity> {
        [Severity::Info, Severity::Warning, Severity::Error]
            .into_iter()
            .find(|s| s.name() == name)
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A lint rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rule {
    /// A comparison with a `NULL` literal, which is never true.
    NullComparison,
    /// An `UPDATE` or `DELETE` with no `WHERE` clause.
    UnboundedWrite,
    /// A `NOT IN (SELECT …)` whose left side or subquery output may be NULL.
    NotInNullable,
    /// A `CREATE INDEX` without `CONCURRENTLY` on an existing table.
    CreateIndexBlocking,
    /// An `ADD CONSTRAINT` / `ADD COLUMN` that validates existing rows.
    ConstraintValidating,
    /// An `ALTER COLUMN … SET NOT NULL` that scans the table.
    SetNotNullScan,
    /// An `ALTER TABLE` that rewrites the whole table under a lock.
    TableRewrite,
    /// A foreign key whose referencing columns have no index.
    FkWithoutIndex,
    /// An index another index on the table already covers.
    RedundantIndex,
    /// A `sql!` binding whose parameter would capture the same variable
    /// without it (`x = x`, or a bare `x`).
    RedundantBinding,
    /// A `sql!` binding that only renames a variable (`p = v`).
    RenamingBinding,
    /// `IS [NOT] NULL` or `COALESCE` on a value that is never NULL.
    RedundantNullCheck,
    /// `LIMIT`, `OFFSET` or `DISTINCT ON` without `ORDER BY`.
    LimitWithoutOrder,
    /// A `LIMIT` on a query that returns at most one row.
    RedundantLimit,
    /// A `DISTINCT` on rows that are already distinct.
    RedundantDistinct,
    /// A `LIKE` pattern built from a parameter whose `%` and `_` are not
    /// escaped.
    LikeUnescapedParam,
    /// A foreign key whose column types differ from the referenced columns.
    FkTypeMismatch,
    /// A migration taking a blocking lock without `SET lock_timeout`.
    MissingLockTimeout,
    /// A `timestamp without time zone` column.
    PreferTimestamptz,
    /// A `serial` column instead of an identity column.
    PreferIdentity,
    /// A `varchar(n)` or `char(n)` column instead of `text`.
    PreferText,
    /// An `int2` or `int4` primary key.
    PreferBigintPk,
    /// `fetch_one` / `fetch_optional` on a query not proven to return at most
    /// one row.
    FetchMayReturnMany,
    /// `fetch_all` on a query that returns at most one row.
    FetchAllSingleRow,
    /// `execute()` on a statement whose `RETURNING` rows are discarded.
    UnusedReturning,
    /// A malformed or unknown rule name in an allow comment. Never
    /// configurable and never suppressible: it is always an error.
    InvalidAllow,
    /// A rule offered a fix that would change the query: a typedpg bug. The
    /// fix is withheld and this warning asks for a report. Never
    /// configurable and never suppressible.
    InternalError,
}

impl Rule {
    /// Every configurable rule.
    pub const ALL: [Rule; 25] = [
        Rule::NullComparison,
        Rule::UnboundedWrite,
        Rule::NotInNullable,
        Rule::CreateIndexBlocking,
        Rule::ConstraintValidating,
        Rule::SetNotNullScan,
        Rule::TableRewrite,
        Rule::FkWithoutIndex,
        Rule::RedundantIndex,
        Rule::RedundantBinding,
        Rule::RenamingBinding,
        Rule::RedundantNullCheck,
        Rule::LimitWithoutOrder,
        Rule::RedundantLimit,
        Rule::RedundantDistinct,
        Rule::LikeUnescapedParam,
        Rule::FkTypeMismatch,
        Rule::MissingLockTimeout,
        Rule::PreferTimestamptz,
        Rule::PreferIdentity,
        Rule::PreferText,
        Rule::PreferBigintPk,
        Rule::FetchMayReturnMany,
        Rule::FetchAllSingleRow,
        Rule::UnusedReturning,
    ];

    /// The rule's configuration key (`[package.metadata.typedpg.lint]`, and
    /// the TypeScript CLI's `lint` object in camelCase).
    pub fn name(self) -> &'static str {
        match self {
            Rule::NullComparison => "null_comparison",
            Rule::UnboundedWrite => "unbounded_write",
            Rule::NotInNullable => "not_in_nullable",
            Rule::CreateIndexBlocking => "create_index_blocking",
            Rule::ConstraintValidating => "constraint_validating",
            Rule::SetNotNullScan => "set_not_null_scan",
            Rule::TableRewrite => "table_rewrite",
            Rule::FkWithoutIndex => "fk_without_index",
            Rule::RedundantIndex => "redundant_index",
            Rule::RedundantBinding => "redundant_binding",
            Rule::RenamingBinding => "renaming_binding",
            Rule::RedundantNullCheck => "redundant_null_check",
            Rule::LimitWithoutOrder => "limit_without_order",
            Rule::RedundantLimit => "redundant_limit",
            Rule::RedundantDistinct => "redundant_distinct",
            Rule::LikeUnescapedParam => "like_unescaped_param",
            Rule::FkTypeMismatch => "fk_type_mismatch",
            Rule::MissingLockTimeout => "missing_lock_timeout",
            Rule::PreferTimestamptz => "prefer_timestamptz",
            Rule::PreferIdentity => "prefer_identity",
            Rule::PreferText => "prefer_text",
            Rule::PreferBigintPk => "prefer_bigint_pk",
            Rule::FetchMayReturnMany => "fetch_may_return_many",
            Rule::FetchAllSingleRow => "fetch_all_single_row",
            Rule::UnusedReturning => "unused_returning",
            Rule::InvalidAllow => "invalid_allow",
            Rule::InternalError => "internal_error",
        }
    }

    /// The configurable rule named `name` (see [`Rule::name`]).
    pub fn from_name(name: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|r| r.name() == name)
    }

    /// A one-line description of what the rule reports, used by the SARIF
    /// rule metadata.
    pub fn description(self) -> &'static str {
        match self {
            Rule::NullComparison => "comparison with NULL is never true",
            Rule::UnboundedWrite => "UPDATE or DELETE without a WHERE clause",
            Rule::NotInNullable => {
                "NOT IN (SELECT …) that returns no rows because a side may be NULL"
            }
            Rule::CreateIndexBlocking => {
                "CREATE INDEX without CONCURRENTLY on a table that already has rows"
            }
            Rule::ConstraintValidating => "a constraint that validates existing rows under a lock",
            Rule::SetNotNullScan => "SET NOT NULL that scans every row of the table",
            Rule::TableRewrite => "ALTER TABLE that rewrites every row under a lock",
            Rule::FkWithoutIndex => "a foreign key whose referencing columns have no index",
            Rule::RedundantIndex => "an index another index on the table already covers",
            Rule::RedundantBinding => "a `sql!` binding the parameter would capture without it",
            Rule::RenamingBinding => "a `sql!` binding that only renames a variable",
            Rule::RedundantNullCheck => "IS [NOT] NULL or COALESCE on a value that is never NULL",
            Rule::LimitWithoutOrder => {
                "LIMIT, OFFSET or DISTINCT ON without ORDER BY picks arbitrary rows"
            }
            Rule::RedundantLimit => "a LIMIT on a query that returns at most one row",
            Rule::RedundantDistinct => "a DISTINCT on rows that are already distinct",
            Rule::LikeUnescapedParam => {
                "a LIKE pattern built from a parameter whose % and _ are not escaped"
            }
            Rule::FkTypeMismatch => {
                "a foreign key whose column types differ from the referenced columns"
            }
            Rule::MissingLockTimeout => {
                "a migration taking a blocking lock on an existing table without SET lock_timeout"
            }
            Rule::PreferTimestamptz => "a timestamp without time zone column",
            Rule::PreferIdentity => "a serial column instead of an identity column",
            Rule::PreferText => "a varchar(n) or char(n) column instead of text",
            Rule::PreferBigintPk => "an int2 or int4 primary key",
            Rule::FetchMayReturnMany => {
                "fetch_one / fetch_optional on a query not proven to return at most one row"
            }
            Rule::FetchAllSingleRow => "fetch_all on a query that returns at most one row",
            Rule::UnusedReturning => "execute() on a statement whose RETURNING rows are discarded",
            Rule::InvalidAllow => "a malformed or unknown rule name in a typedpg-lint comment",
            Rule::InternalError => "a typedpg bug: a fix that would have changed the query",
        }
    }

    /// The severity the rule reports when enabled, whether or not it is on by
    /// default.
    pub fn default_severity(self) -> Severity {
        self.default_level().unwrap_or(Severity::Info)
    }

    /// The severity the rule reports unless the configuration overrides it,
    /// or `None` when the rule is off by default.
    pub fn default_level(self) -> Option<Severity> {
        match self {
            Rule::NullComparison | Rule::UnboundedWrite | Rule::InvalidAllow => {
                Some(Severity::Error)
            }
            Rule::NotInNullable
            | Rule::CreateIndexBlocking
            | Rule::ConstraintValidating
            | Rule::SetNotNullScan
            | Rule::TableRewrite
            | Rule::FkWithoutIndex
            | Rule::RedundantBinding
            | Rule::InternalError
            | Rule::RedundantNullCheck
            | Rule::LimitWithoutOrder
            | Rule::FkTypeMismatch
            | Rule::MissingLockTimeout
            | Rule::FetchMayReturnMany => Some(Severity::Warning),
            Rule::RedundantIndex
            | Rule::RenamingBinding
            | Rule::RedundantLimit
            | Rule::RedundantDistinct
            | Rule::FetchAllSingleRow
            | Rule::UnusedReturning => Some(Severity::Info),
            // Off by default: enabled with a level, they report at info.
            Rule::LikeUnescapedParam
            | Rule::PreferTimestamptz
            | Rule::PreferIdentity
            | Rule::PreferText
            | Rule::PreferBigintPk => None,
        }
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// An edit to the SQL text, as a fix for a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlEdit {
    /// The byte range to replace: in the analyzed SQL text for an internal
    /// finding, and in the original query text for a [`Finding`].
    pub range: Range<usize>,
    /// The replacement text.
    pub text: String,
}

/// The method a `sql!` result has called on it directly, as the Rust source
/// shows it to the CLI.
///
/// A query that is first bound to a variable has no call site: the binding
/// hides it from [`lint_query_called`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallMethod {
    /// `fetch_one()` or `fetch_one_as()`: fails with `TooManyRows` on a
    /// second row.
    FetchOne,
    /// `fetch_optional()` or `fetch_optional_as()`: fails with
    /// `TooManyRows` on a second row.
    FetchOptional,
    /// `fetch_all()` or `fetch_all_as()`.
    FetchAll,
    /// `execute()`: runs the statement and discards its `RETURNING` rows.
    Execute,
}

/// A lint finding, placed in the original query text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: Rule,
    pub severity: Severity,
    /// The byte range in the query text as the user wrote it (`$name`
    /// parameters and `$..spreads` included), or in the migration file's
    /// text for a [`MigrationFinding`].
    pub span: Range<usize>,
    pub message: String,
    pub help: Option<String>,
    /// The edits that fix the finding, in the query text as the user wrote
    /// it; empty when the finding has no fix. All edits of one finding are
    /// applied together or not at all, and must not overlap.
    pub fix: Vec<SqlEdit>,
}

impl Finding {
    /// The severity's line in [`render`]'s format:
    /// `error[null_comparison]`.
    pub fn label(&self) -> String {
        format!("{}[{}]", self.severity, self.rule)
    }
}

/// Every rule's effective level, and the severity a run fails on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintOptions {
    levels: BTreeMap<Rule, Option<Severity>>,
    fail_on: Severity,
}

impl Default for LintOptions {
    fn default() -> Self {
        Self {
            levels: Rule::ALL
                .into_iter()
                .map(|rule| (rule, rule.default_level()))
                .collect(),
            fail_on: Severity::Error,
        }
    }
}

impl LintOptions {
    /// The options `[package.metadata.typedpg.lint]` (or the TypeScript
    /// config's `lint` object) configures: rules without an override keep
    /// their [`Rule::default_level`], and `fail_on` defaults to error.
    pub fn from_config(config: &LintConfig) -> Self {
        let mut options = LintOptions {
            fail_on: severity(config.fail_on),
            ..LintOptions::default()
        };
        for (name, level) in &config.levels {
            if let Some(rule) = Rule::from_name(name) {
                options.levels.insert(rule, level.severity().map(severity));
            }
        }
        options
    }

    /// The severity `rule`'s findings get, or `None` when it is off.
    pub fn level(&self, rule: Rule) -> Option<Severity> {
        self.levels.get(&rule).copied().flatten()
    }

    /// The severity a finding must reach to fail the run.
    pub fn fail_on(&self) -> Severity {
        self.fail_on
    }
}

fn severity(severity: typedpg_core::config::LintSeverity) -> Severity {
    match severity {
        typedpg_core::config::LintSeverity::Info => Severity::Info,
        typedpg_core::config::LintSeverity::Warning => Severity::Warning,
        typedpg_core::config::LintSeverity::Error => Severity::Error,
    }
}

/// A byte range in the analyzed text, before mapping back to the original.
pub(crate) struct RawFinding {
    pub span: Range<usize>,
    pub message: String,
    pub help: Option<String>,
    /// The fixes, with ranges in the analyzed text.
    pub fix: Vec<SqlEdit>,
}

/// A non-comment token of the analyzed text.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tok {
    pub start: usize,
    pub end: usize,
}

/// What the rules see.
pub(crate) struct Ctx<'a> {
    /// The analyzed text (the [`typedpg_analyzer::QueryView`]'s SQL).
    pub sql: &'a str,
    pub tree: &'a typedpg_pg_query::protobuf::ParseResult,
    /// The non-comment tokens of `sql`, in order.
    pub tokens: &'a [Tok],
    /// The names of operators with a non-strict implementation: comparing
    /// with `NULL` through one may be meaningful.
    pub non_strict_operators: &'a HashSet<String>,
    /// The analyzed view, for the facts the analyzer proved (the sublink
    /// nullability the `not_in_nullable` rule reads).
    pub view: &'a typedpg_analyzer::QueryView,
    /// The catalog the query was analyzed against.
    pub catalog: &'a PgCatalog,
}

/// Lint `sql` against `catalog`.
///
/// Returns the findings in the text as written, or the analysis error
/// exactly as `typedpg check` reports it.
pub fn lint_query(
    catalog: &PgCatalog,
    sql: &str,
    options: &LintOptions,
) -> Result<Vec<Finding>, LocatedError> {
    lint_query_called(catalog, sql, None, options)
}

/// [`lint_query`], with the method called on the `sql!` result when the
/// caller can see the Rust source (`cargo typedpg lint` can; the TypeScript
/// CLI cannot).
///
/// The call-site rules (`fetch_may_return_many`, `fetch_all_single_row` and
/// `unused_returning`) read `call` and the same
/// [`QueryView`](typedpg_analyzer::QueryView) every other query rule uses,
/// so no query is analyzed twice. Their findings are placed in the query
/// text like any other.
pub fn lint_query_called(
    catalog: &PgCatalog,
    sql: &str,
    call: Option<CallMethod>,
    options: &LintOptions,
) -> Result<Vec<Finding>, LocatedError> {
    let view = catalog.query_view(sql)?;
    let tokens: Vec<Tok> = typedpg_pg_query::scan(&view.sql)
        .map(|scanned| {
            scanned
                .tokens
                .iter()
                .filter(|token| !matches!(token.token(), Token::SqlComment | Token::CComment))
                .filter_map(|token| {
                    Some(Tok {
                        start: usize::try_from(token.start).ok()?,
                        end: usize::try_from(token.end).ok()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let (suppressed, mut findings) = suppressions(&view.sql);
    let non_strict_operators = non_strict_operators(catalog);
    let ctx = Ctx {
        sql: &view.sql,
        tree: &view.tree,
        tokens: &tokens,
        non_strict_operators: &non_strict_operators,
        view: &view,
        catalog,
    };
    for rule in Rule::ALL {
        let Some(severity) = options.level(rule) else {
            continue;
        };
        if suppressed.contains(&rule) {
            continue;
        }
        for raw in rules::findings(rule, &ctx)
            .into_iter()
            .chain(call_site::findings(rule, &ctx, call))
        {
            findings.push(Finding {
                rule,
                severity,
                span: raw.span,
                message: raw.message,
                help: raw.help,
                fix: raw.fix,
            });
        }
    }
    for finding in &mut findings {
        finding.span = view.original_span(finding.span.clone());
        for edit in &mut finding.fix {
            edit.range = view.original_span(edit.range.clone());
        }
    }
    let mut bugs = Vec::new();
    for finding in &mut findings {
        if finding.fix.is_empty() {
            continue;
        }
        // `unused_returning` drops the RETURNING its call site discards: its
        // fix is the one meant to leave no output column.
        let columns = if finding.rule == Rule::UnusedReturning {
            ExpectedColumns::None
        } else {
            ExpectedColumns::Same
        };
        if let Err(change) = fix_keeps_shape(catalog, sql, &view.analyzed, &finding.fix, columns) {
            finding.fix.clear();
            let message = format!(
                "typedpg bug: the {} fix would have changed {change}, so it was not offered",
                finding.rule
            );
            // A rule's own tests must never get here.
            if cfg!(test) {
                panic!("{message}: {sql}");
            }
            bugs.push(Finding {
                rule: Rule::InternalError,
                severity: Severity::Warning,
                span: finding.span.clone(),
                message,
                help: Some(
                    "please report it at https://github.com/cubos/typedpg/issues with this query"
                        .to_owned(),
                ),
                fix: Vec::new(),
            });
        }
    }
    findings.extend(bugs);
    // A deterministic order within a query: by position, then rule.
    findings.sort_by(|a, b| {
        (a.span.start, a.span.end, a.rule).cmp(&(b.span.start, b.span.end, b.rule))
    });
    Ok(findings)
}

/// Whether `sql` with `fix` applied still analyzes to the same query shape
/// as `original`: the same output columns (name, type, nullability), the
/// same parameters (name, type, nullability) and the same embeddability.
/// The error says what would have changed.
///
/// A safety net under every rule's own proof that its fix keeps the query's
/// meaning: no rule should ever propose a fix that fails it, so a failure is
/// a typedpg bug. It can't see a change in the rows themselves; that stays
/// each rule's proof to make.
fn fix_keeps_shape(
    catalog: &PgCatalog,
    sql: &str,
    original: &typedpg_analyzer::AnalyzedQuery,
    fix: &[SqlEdit],
    columns: ExpectedColumns,
) -> Result<(), &'static str> {
    let edited = apply_edits(sql, fix).ok_or("the query text (overlapping edits)")?;
    let analyzed = catalog
        .analyze(&edited)
        .map_err(|_| "the query into one that doesn't analyze")?;
    let same_columns = match columns {
        ExpectedColumns::None => analyzed.columns.is_empty(),
        ExpectedColumns::Same => {
            analyzed.columns.len() == original.columns.len()
                && analyzed
                    .columns
                    .iter()
                    .zip(&original.columns)
                    .all(|(a, b)| {
                        a.name == b.name && a.pg_type == b.pg_type && a.nullable == b.nullable
                    })
        }
    };
    if !same_columns {
        return Err("its output columns");
    }
    let same_params =
        analyzed.params.len() == original.params.len()
            && analyzed.params.iter().zip(&original.params).all(|(a, b)| {
                a.name == b.name && a.pg_type == b.pg_type && a.nullable == b.nullable
            });
    if !same_params {
        return Err("its parameters");
    }
    if analyzed.can_run_as_subquery != original.can_run_as_subquery {
        return Err("whether it can run as a subquery");
    }
    Ok(())
}

/// The output columns a fixed query must have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedColumns {
    /// The original query's, unchanged.
    Same,
    /// None: the fix removes a RETURNING whose rows the caller discards.
    None,
}

/// `text` with `edits` applied, or `None` when they overlap or fall outside
/// it.
fn apply_edits(text: &str, edits: &[SqlEdit]) -> Option<String> {
    let mut edits: Vec<&SqlEdit> = edits.iter().collect();
    edits.sort_by_key(|edit| edit.range.start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for edit in edits {
        if edit.range.start < at || edit.range.end < edit.range.start {
            return None;
        }
        out.push_str(text.get(at..edit.range.start)?);
        out.push_str(&edit.text);
        at = edit.range.end;
    }
    out.push_str(text.get(at..)?);
    Some(out)
}

/// Whether any finding is at or above the configured `fail_on` severity.
/// Errors from an allow comment always are.
pub fn fails(findings: &[Finding], options: &LintOptions) -> bool {
    findings
        .iter()
        .any(|finding| finding.severity >= options.fail_on)
}

/// The operator names with an implementation that is not strict
/// (`proisstrict = false`): a comparison through one may accept `NULL`.
fn non_strict_operators(catalog: &PgCatalog) -> HashSet<String> {
    let strict: HashMap<_, bool> = catalog
        .iter_procs()
        .map(|proc| (proc.oid, proc.proisstrict))
        .collect();
    catalog
        .iter_operators()
        .filter(|operator| {
            operator
                .oprcode
                .is_some_and(|code| strict.get(&code).is_some_and(|strict| !strict))
        })
        .map(|operator| operator.oprname.clone())
        .collect()
}

/// Find the allow comments of `sql`, returning the rules they suppress and
/// the error findings for unknown rule names.
fn suppressions(sql: &str) -> (HashSet<Rule>, Vec<Finding>) {
    let mut suppressed = HashSet::new();
    let mut errors = Vec::new();
    let Ok(scanned) = typedpg_pg_query::scan(sql) else {
        return (suppressed, errors);
    };
    for token in &scanned.tokens {
        if !matches!(token.token(), Token::SqlComment | Token::CComment) {
            continue;
        }
        let (Ok(start), Ok(end)) = (usize::try_from(token.start), usize::try_from(token.end))
        else {
            continue;
        };
        let names = match allow_comment(&sql[start..end]) {
            AllowComment::None => continue,
            AllowComment::Allow(names) => names,
            AllowComment::Malformed => {
                errors.push(Finding {
                    rule: Rule::InvalidAllow,
                    severity: Severity::Error,
                    span: start..end,
                    message: "malformed typedpg-lint comment: expected \
                              `typedpg-lint: allow(rule, ...)` and nothing after it"
                        .to_owned(),
                    help: None,
                    fix: Vec::new(),
                });
                continue;
            }
        };
        for name in names {
            match Rule::from_name(&name) {
                Some(rule) => {
                    suppressed.insert(rule);
                }
                None => {
                    let valid = Rule::ALL
                        .iter()
                        .map(|rule| rule.name())
                        .collect::<Vec<_>>()
                        .join(", ");
                    errors.push(Finding {
                        rule: Rule::InvalidAllow,
                        severity: Severity::Error,
                        span: start..end,
                        message: format!(
                            "unknown lint rule `{name}` in a typedpg-lint allow comment: \
                             expected one of {valid}"
                        ),
                        help: None,
                        fix: Vec::new(),
                    });
                }
            }
        }
    }
    (suppressed, errors)
}

/// The rules the `typedpg-lint: allow(...)` comments of `sql` suppress.
///
/// A `sql!` template's binding rules run over its Rust source, outside
/// [`lint_query`], so the CLI reads the template's own allow comments
/// through this. Unknown names are not reported here: the query analysis
/// reports them as [`Rule::InvalidAllow`].
pub fn allowed_rules(sql: &str) -> HashSet<Rule> {
    suppressions(sql).0
}

/// What a comment says to the linter.
enum AllowComment {
    /// Not a `typedpg-lint:` comment.
    None,
    /// `typedpg-lint: allow(a, b)`: the rule names as written.
    Allow(Vec<String>),
    /// A `typedpg-lint:` comment that isn't `allow(…)` with nothing after.
    Malformed,
}

/// Read `comment` (with its `--` or `/* */` delimiters).
fn allow_comment(comment: &str) -> AllowComment {
    let inner = if let Some(rest) = comment.strip_prefix("--") {
        rest
    } else if let Some(rest) = comment.strip_prefix("/*") {
        rest.strip_suffix("*/").unwrap_or(rest)
    } else {
        return AllowComment::None;
    };
    let Some(rest) = inner.trim().strip_prefix("typedpg-lint:") else {
        return AllowComment::None;
    };
    let names = rest
        .trim()
        .strip_prefix("allow")
        .map(str::trim_start)
        .and_then(|list| list.strip_prefix('('))
        .and_then(|list| list.strip_suffix(')'));
    match names {
        Some(list) => AllowComment::Allow(
            list.split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
        None => AllowComment::Malformed,
    }
}

/// `path:line:column: severity[rule]: message`, and the help line when the
/// finding has one:
///
/// ```text
/// src/users.ts:12:30: error[null_comparison]: comparison with NULL is never true
///   help: use IS NULL / IS NOT NULL
/// ```
pub fn render(path: &Path, line: u32, column: u32, finding: &Finding) -> String {
    let mut out = format!(
        "{}:{}:{}: {}: {}",
        path.display(),
        line,
        column,
        finding.label(),
        finding.message
    );
    if let Some(help) = &finding.help {
        out.push_str(&format!("\n  help: {help}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use typedpg_analyzer::PgCatalog;
    use typedpg_core::config::{LINT_RULES, LintLevel, LintSeverity, RUST_LINT_RULES};

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (id integer PRIMARY KEY, name text, email text);
                 CREATE TABLE posts (id integer PRIMARY KEY, author_id integer, title text);
                 CREATE TABLE \"My Users\" (id integer PRIMARY KEY, name text);",
            )
            .unwrap();
        catalog
    }

    fn lint(catalog: &PgCatalog, sql: &str) -> Vec<Finding> {
        lint_query(catalog, sql, &LintOptions::default()).unwrap()
    }

    /// Whether deleting `remove` (each a substring of `sql`, found from the
    /// left) keeps the query's shape.
    fn keeps_shape(catalog: &PgCatalog, sql: &str, remove: &[&str]) -> bool {
        let original = catalog.analyze(sql).unwrap();
        let fix: Vec<SqlEdit> = remove
            .iter()
            .map(|part| {
                let start = sql.find(part).unwrap();
                SqlEdit {
                    range: start..start + part.len(),
                    text: String::new(),
                }
            })
            .collect();
        fix_keeps_shape(catalog, sql, &original, &fix, ExpectedColumns::Same).is_ok()
    }

    #[test]
    fn a_fix_keeping_the_shape_is_kept() {
        let db = catalog();
        assert!(keeps_shape(
            &db,
            "SELECT DISTINCT id FROM users WHERE id = $id",
            &["DISTINCT "]
        ));
        assert!(keeps_shape(
            &db,
            "SELECT id FROM users WHERE id = $id LIMIT 1",
            &[" LIMIT 1"]
        ));
    }

    #[test]
    fn a_fix_changing_the_shape_is_dropped() {
        let db = catalog();
        // The implicit column name `coalesce` would become `id`.
        assert!(!keeps_shape(
            &db,
            "SELECT COALESCE(id, 0) FROM users",
            &["COALESCE(", ", 0)"]
        ));
        // The bigint result would become integer.
        assert!(!keeps_shape(
            &db,
            "SELECT COALESCE(id, 0::int8) AS v FROM users",
            &["COALESCE(", ", 0::int8)"]
        ));
        // Nullability: name may be NULL, the COALESCE never is.
        assert!(!keeps_shape(
            &db,
            "SELECT COALESCE(name, '') AS v FROM users",
            &["COALESCE(", ", '')"]
        ));
        // A parameter's type.
        assert!(!keeps_shape(
            &db,
            "SELECT id FROM users WHERE id = $id::int8",
            &["::int8"]
        ));
        // A broken query.
        assert!(!keeps_shape(&db, "SELECT id FROM users", &["FROM "]));
        // Overlapping edits.
        let original = db.analyze("SELECT id FROM users").unwrap();
        let overlapping = [
            SqlEdit {
                range: 0..8,
                text: String::new(),
            },
            SqlEdit {
                range: 4..10,
                text: String::new(),
            },
        ];
        assert!(
            fix_keeps_shape(
                &db,
                "SELECT id FROM users",
                &original,
                &overlapping,
                ExpectedColumns::Same
            )
            .is_err()
        );
    }

    #[test]
    fn a_fix_dropping_returning_must_leave_no_column() {
        let db = catalog();
        let sql = "DELETE FROM users WHERE id = $id RETURNING id";
        let original = db.analyze(sql).unwrap();
        let start = sql.find(" RETURNING").unwrap();
        let drop_returning = [SqlEdit {
            range: start..sql.len(),
            text: String::new(),
        }];
        assert!(
            fix_keeps_shape(&db, sql, &original, &drop_returning, ExpectedColumns::None).is_ok()
        );
        assert!(
            fix_keeps_shape(&db, sql, &original, &drop_returning, ExpectedColumns::Same).is_err()
        );
        // Keeping a column is not what such a fix may do.
        let keep = [SqlEdit {
            range: start..start,
            text: String::new(),
        }];
        assert!(fix_keeps_shape(&db, sql, &original, &keep, ExpectedColumns::None).is_err());
    }

    fn options(level: Option<Severity>, fail_on: Option<Severity>) -> LintOptions {
        let mut config = LintConfig::default();
        config.levels.insert(
            "null_comparison".to_owned(),
            match level {
                None => LintLevel::Off,
                Some(Severity::Info) => LintLevel::Info,
                Some(Severity::Warning) => LintLevel::Warning,
                Some(Severity::Error) => LintLevel::Error,
            },
        );
        if let Some(fail_on) = fail_on {
            config.fail_on = match fail_on {
                Severity::Info => LintSeverity::Info,
                Severity::Warning => LintSeverity::Warning,
                Severity::Error => LintSeverity::Error,
            };
        }
        LintOptions::from_config(&config)
    }

    /// The config lists name every rule, each in `Rule::ALL`'s order: the
    /// query, migration and schema rules in [`LINT_RULES`], the Rust-only
    /// ones in [`RUST_LINT_RULES`]. The two lists are not concatenated in
    /// `Rule::ALL`'s order (the two original Rust rules come before the new
    /// rules), so each list is checked as a subsequence.
    fn assert_rule_lists_match() {
        let names: Vec<&str> = Rule::ALL.iter().map(|rule| rule.name()).collect();
        let lint: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| LINT_RULES.contains(name))
            .collect();
        assert_eq!(lint, LINT_RULES);
        let rust: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| RUST_LINT_RULES.contains(name))
            .collect();
        assert_eq!(rust, RUST_LINT_RULES);
        assert_eq!(
            lint.len() + rust.len(),
            names.len(),
            "a rule is in neither config list"
        );
    }

    #[test]
    fn config_rule_names_match_the_lint_crate() {
        assert_rule_lists_match();
    }

    // ── null_comparison: positive shapes ───────────────────────────────────

    #[test]
    fn null_comparison_every_operator_and_operand_side() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE id = NULL",
            "SELECT id FROM users WHERE id <> NULL",
            "SELECT id FROM users WHERE id != NULL",
            "SELECT id FROM users WHERE id < NULL",
            "SELECT id FROM users WHERE id > NULL",
            "SELECT id FROM users WHERE id <= NULL",
            "SELECT id FROM users WHERE id >= NULL",
            "SELECT id FROM users WHERE NULL = id",
            "SELECT id FROM users WHERE NULL <> id",
            "SELECT id FROM users WHERE id = NULL::int",
            "SELECT id FROM users WHERE NULL::int < id",
            "SELECT id FROM users WHERE id OPERATOR(pg_catalog.=) NULL",
        ] {
            let findings = lint(&db, sql);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(findings[0].rule, Rule::NullComparison);
            assert_eq!(findings[0].severity, Severity::Error);
            assert_eq!(findings[0].message, "comparison with NULL is never true");
            assert_eq!(
                findings[0].help.as_deref(),
                Some("use IS NULL / IS NOT NULL")
            );
        }
    }

    #[test]
    fn null_comparison_is_found_everywhere() {
        let db = catalog();
        for sql in [
            "SELECT (id = NULL) AS bad FROM users",
            "SELECT id FROM users WHERE id IN (SELECT author_id FROM posts WHERE title = NULL)",
            "WITH bad AS (SELECT id FROM users WHERE name = NULL) SELECT * FROM bad",
            "SELECT u.id FROM users u JOIN posts p ON u.id = p.author_id AND p.title = NULL",
            "SELECT count(*) FROM users GROUP BY email HAVING count(*) = NULL",
            "SELECT CASE WHEN id = NULL THEN 1 ELSE 0 END FROM users",
            "SELECT id FROM users UNION ALL SELECT id FROM posts WHERE author_id = NULL",
            "INSERT INTO posts (id, author_id, title) SELECT id, id, NULL FROM users RETURNING (SELECT id FROM users WHERE email = NULL LIMIT 1)",
            "WITH changed AS (UPDATE posts SET title = 'x' WHERE title = NULL RETURNING id) SELECT * FROM changed",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.rule == Rule::NullComparison),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn not_in_with_a_null_is_flagged() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE id NOT IN (1, NULL)",
            "SELECT id FROM users WHERE id NOT IN (NULL::int, 2)",
            "SELECT id FROM users WHERE id NOT IN (SELECT author_id FROM posts) AND id NOT IN (1, 2, NULL)",
        ] {
            let findings = lint(&db, sql);
            let null_findings: Vec<_> = findings
                .iter()
                .filter(|finding| finding.rule == Rule::NullComparison)
                .collect();
            assert_eq!(null_findings.len(), 1, "{sql}: {findings:?}");
            let finding = null_findings[0];
            assert_eq!(
                finding.message,
                "NOT IN with a NULL in the list is never true"
            );
            assert_eq!(
                finding.help.as_deref(),
                Some("remove the NULL from the list")
            );
        }
        let sql = "SELECT id FROM users WHERE id NOT IN (1, NULL)";
        let findings = lint(&db, sql);
        assert_eq!(&sql[findings[0].span.clone()], "id NOT IN (1, NULL)");
    }

    #[test]
    fn simple_case_with_null_is_flagged() {
        let db = catalog();
        let sql = "SELECT CASE id WHEN NULL THEN 'null' ELSE 'other' END FROM users";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].message, "CASE ... WHEN NULL never matches");
        assert_eq!(findings[0].help.as_deref(), Some("use CASE WHEN x IS NULL"));
        assert_eq!(&sql[findings[0].span.clone()], "WHEN NULL");
    }

    #[test]
    fn null_comparison_never_flags_the_safe_forms() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE email IS NULL",
            "SELECT id FROM users WHERE email IS NOT NULL",
            "SELECT id FROM users WHERE email IS DISTINCT FROM NULL",
            "SELECT id FROM users WHERE email IS NOT DISTINCT FROM NULL",
            "SELECT id FROM users WHERE id IN (1, NULL)",
            "SELECT id FROM users WHERE id NOT IN (1, 2)",
            "SELECT id FROM users WHERE id NOT IN (SELECT author_id FROM posts)",
            "SELECT coalesce(email, NULL) FROM users",
            "SELECT id FROM users WHERE id = ANY(ARRAY[1, 2])",
            "SELECT id FROM users WHERE id = ANY(SELECT author_id FROM posts)",
            "SELECT NULLIF(id, NULL) FROM users",
            "SELECT CASE WHEN email IS NULL THEN 'none' ELSE email END FROM users",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::NullComparison),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn spans_are_mapped_past_params_and_spreads() {
        let db = catalog();
        let sql = "SELECT (SELECT count(*) FROM users WHERE id IN $..ids) AS c, \
                   (SELECT id FROM users WHERE id = $id AND email = NULL LIMIT 1) AS n";
        let findings: Vec<Finding> = lint(&db, sql)
            .into_iter()
            .filter(|finding| finding.rule == Rule::NullComparison)
            .collect();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "email = NULL");

        let sql = "SELECT id FROM users WHERE name = $name AND id = NULL::bigint";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "id = NULL::bigint");
    }

    #[test]
    fn non_strict_user_operator_is_not_flagged() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TYPE weird AS (v integer);
             CREATE FUNCTION weird_eq(weird, weird) RETURNS boolean
                 LANGUAGE sql AS 'SELECT true';
             CREATE OPERATOR = (LEFTARG = weird, RIGHTARG = weird, FUNCTION = weird_eq);",
        )
        .unwrap();
        // The built-in `=` stays strict; the user-defined one is CALLED ON
        // NULL INPUT, so the name `=` is not trusted.
        let findings = lint(&db, "SELECT NULL::weird = NULL::weird");
        assert!(
            findings
                .iter()
                .all(|finding| finding.rule != Rule::NullComparison),
            "{findings:?}"
        );
    }

    // ── unbounded_write ────────────────────────────────────────────────────

    #[test]
    fn unbounded_write_is_flagged_everywhere() {
        let db = catalog();
        for (sql, message) in [
            (
                "UPDATE users SET name = 'x'",
                "UPDATE without WHERE changes every row of users",
            ),
            (
                "DELETE FROM users",
                "DELETE without WHERE removes every row of users",
            ),
            (
                "WITH gone AS (DELETE FROM posts RETURNING id) SELECT * FROM gone",
                "DELETE without WHERE removes every row of posts",
            ),
            (
                "WITH changed AS (UPDATE public.users SET name = 'x' RETURNING id) SELECT * FROM changed",
                "UPDATE without WHERE changes every row of public.users",
            ),
            (
                "UPDATE \"My Users\" SET name = 'x'",
                "UPDATE without WHERE changes every row of \"My Users\"",
            ),
        ] {
            let findings = lint(&db, sql);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(findings[0].rule, Rule::UnboundedWrite);
            assert_eq!(findings[0].message, message);
            assert_eq!(
                findings[0].help.as_deref(),
                Some(
                    "add a WHERE clause, or allow it with -- typedpg-lint: \
                     allow(unbounded_write)"
                )
            );
        }
        let findings = lint(&db, "UPDATE users SET name = 'x'");
        assert_eq!(
            &"UPDATE users SET name = 'x'"[findings[0].span.clone()],
            "UPDATE users"
        );
    }

    #[test]
    fn bounded_writes_are_not_flagged() {
        let db = catalog();
        for sql in [
            "UPDATE users SET name = 'x' WHERE id = 1",
            "DELETE FROM users WHERE id = 1",
            "UPDATE users SET name = 'x' WHERE CURRENT OF some_cursor",
            "WITH changed AS (UPDATE users SET name = 'x' WHERE id = 1 RETURNING id) SELECT * FROM changed",
            "DELETE FROM users USING posts WHERE users.id = posts.author_id",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::UnboundedWrite),
                "{sql}: {findings:?}"
            );
        }
    }

    // ── suppression ────────────────────────────────────────────────────────

    #[test]
    fn allow_comments_suppress_rules() {
        let db = catalog();
        for sql in [
            "-- typedpg-lint: allow(null_comparison)\nSELECT id FROM users WHERE email = NULL",
            "/* typedpg-lint: allow(null_comparison) */ SELECT id FROM users WHERE email = NULL",
            "SELECT id FROM users /* typedpg-lint: allow(null_comparison) */ WHERE email = NULL",
            "-- typedpg-lint: allow(null_comparison, unbounded_write)\nUPDATE users SET email = NULL",
        ] {
            let findings = lint(&db, sql);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn allowed_rules_reads_the_allow_comments() {
        assert_eq!(
            allowed_rules("-- typedpg-lint: allow(redundant_binding, renaming_binding)\nSELECT 1"),
            HashSet::from([Rule::RedundantBinding, Rule::RenamingBinding])
        );
        assert!(allowed_rules("SELECT 1").is_empty());
    }

    #[test]
    fn a_dash_comment_inside_a_string_is_not_a_comment() {
        let db = catalog();
        let sql = "SELECT '-- typedpg-lint: allow(null_comparison)' AS s, id \
                   FROM users WHERE email = NULL";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule, Rule::NullComparison);
    }

    #[test]
    fn unknown_rule_in_allow_is_an_error_and_suppresses_nothing() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(no_such_rule)\nSELECT id FROM users WHERE email = NULL";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 2, "{findings:?}");
        let error = findings
            .iter()
            .find(|finding| finding.rule == Rule::InvalidAllow)
            .unwrap();
        assert_eq!(error.severity, Severity::Error);
        assert!(error.message.contains("unknown lint rule `no_such_rule`"));
        assert!(error.message.contains("null_comparison, unbounded_write"));
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule == Rule::NullComparison)
        );
        // Always fails, whatever `fail_on` says — it cannot be configured.
        assert!(fails(
            &findings,
            &options(Some(Severity::Error), Some(Severity::Error))
        ));
    }

    #[test]
    fn malformed_allow_comment_is_an_error_and_suppresses_nothing() {
        let db = catalog();
        for sql in [
            "-- typedpg-lint: allow(null_comparison) -- known\nSELECT id FROM users WHERE email = NULL",
            "-- typedpg-lint: alow(null_comparison)\nSELECT id FROM users WHERE email = NULL",
            "/* typedpg-lint: allow null_comparison */ SELECT id FROM users WHERE email = NULL",
        ] {
            let findings = lint(&db, sql);
            assert_eq!(findings.len(), 2, "{sql}: {findings:?}");
            let error = findings
                .iter()
                .find(|finding| finding.rule == Rule::InvalidAllow)
                .unwrap();
            assert!(
                error.message.starts_with("malformed typedpg-lint comment"),
                "{sql}: {error:?}"
            );
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.rule == Rule::NullComparison),
                "{sql}: {findings:?}"
            );
        }
        // Other comments are not the linter's.
        let findings = lint(&db, "-- typedpg lint\nSELECT id FROM users");
        assert!(findings.is_empty(), "{findings:?}");
    }

    // ── levels and fail_on ─────────────────────────────────────────────────

    #[test]
    fn levels_override_and_turn_rules_off() {
        let db = catalog();
        let sql = "SELECT id FROM users WHERE email = NULL";
        let warning = lint_query(&db, sql, &options(Some(Severity::Warning), None)).unwrap();
        assert_eq!(warning.len(), 1);
        assert_eq!(warning[0].severity, Severity::Warning);
        let info = lint_query(&db, sql, &options(Some(Severity::Info), None)).unwrap();
        assert_eq!(info[0].severity, Severity::Info);
        let off = lint_query(&db, sql, &options(None, None)).unwrap();
        assert!(off.is_empty());
    }

    #[test]
    fn fail_on_decides_the_exit_status() {
        let db = catalog();
        let sql = "SELECT id FROM users WHERE email = NULL";
        let warning = lint_query(&db, sql, &options(Some(Severity::Warning), None)).unwrap();
        assert!(!fails(
            &warning,
            &options(Some(Severity::Warning), Some(Severity::Error))
        ));
        assert!(fails(
            &warning,
            &options(Some(Severity::Warning), Some(Severity::Warning))
        ));
        assert!(fails(
            &warning,
            &options(Some(Severity::Warning), Some(Severity::Info))
        ));
        assert!(!fails(&[], &options(None, Some(Severity::Info))));
    }

    #[test]
    fn render_and_summary_match_the_cli_format() {
        let finding = Finding {
            rule: Rule::NullComparison,
            severity: Severity::Error,
            span: 0..1,
            message: "comparison with NULL is never true".to_owned(),
            help: Some("use IS NULL / IS NOT NULL".to_owned()),
            fix: Vec::new(),
        };
        assert_eq!(
            render(Path::new("src/users.ts"), 12, 30, &finding),
            "src/users.ts:12:30: error[null_comparison]: comparison with NULL is never true\n  \
             help: use IS NULL / IS NOT NULL"
        );
        let mut warning = finding.clone();
        warning.severity = Severity::Warning;
        let mut info = finding.clone();
        info.severity = Severity::Info;
        info.help = None;
        let entry = |finding: Finding| report::Entry {
            path: "src/users.ts".to_owned(),
            start: report::Position {
                line: 12,
                column: 30,
            },
            end: report::Position {
                line: 12,
                column: 36,
            },
            finding,
        };
        let report = report::Report {
            entries: vec![entry(finding), entry(warning), entry(info)],
            ..report::Report::default()
        };
        assert_eq!(
            report.summary(),
            "typedpg lint: 1 error, 1 warning, 1 info\n  checked 0 queries and 0 migrations in 0.00s\n  ✖ failed: 1 finding at or above fail_on (error)"
        );
    }

    #[test]
    fn analysis_errors_come_back_as_errors() {
        let db = catalog();
        let error = lint_query(&db, "SELECT nope FROM users", &LintOptions::default()).unwrap_err();
        assert!(error.error.to_string().contains("column \"nope\""));
        assert!(error.offset.is_some());
    }

    #[test]
    fn rule_names_round_trip() {
        for rule in Rule::ALL {
            assert_eq!(Rule::from_name(rule.name()), Some(rule));
        }
        assert_eq!(Rule::from_name("invalid_allow"), None);
        assert_eq!(Rule::from_name("nope"), None);
        assert_eq!(Rule::InvalidAllow.name(), "invalid_allow");
        assert_eq!(Rule::InvalidAllow.default_severity(), Severity::Error);
    }

    #[test]
    fn off_rules_default_off_and_enable_with_a_level() {
        let options = LintOptions::default();
        for rule in [
            Rule::LikeUnescapedParam,
            Rule::PreferTimestamptz,
            Rule::PreferIdentity,
            Rule::PreferText,
            Rule::PreferBigintPk,
        ] {
            assert_eq!(rule.default_level(), None, "{rule}");
            assert_eq!(rule.default_severity(), Severity::Info, "{rule}");
            assert_eq!(options.level(rule), None, "{rule}");
        }
        let mut config = LintConfig::default();
        config
            .levels
            .insert("like_unescaped_param".to_owned(), LintLevel::Warning);
        let options = LintOptions::from_config(&config);
        assert_eq!(
            options.level(Rule::LikeUnescapedParam),
            Some(Severity::Warning)
        );
    }

    // ── not_in_nullable ────────────────────────────────────────────────────

    fn not_in_findings(db: &PgCatalog, sql: &str) -> Vec<Finding> {
        lint(db, sql)
            .into_iter()
            .filter(|finding| finding.rule == Rule::NotInNullable)
            .collect()
    }

    #[test]
    fn not_in_nullable_reports_the_nullable_side() {
        let db = catalog();
        let cases = [
            (
                "SELECT id FROM users WHERE email NOT IN (SELECT id::text FROM users)",
                "the left side is NULL",
            ),
            (
                "SELECT id FROM users WHERE id NOT IN (SELECT author_id FROM posts)",
                "the subquery returns a NULL",
            ),
            (
                "SELECT id FROM users WHERE email NOT IN (SELECT title FROM posts)",
                "either side is NULL",
            ),
        ];
        for (sql, side) in cases {
            let findings = not_in_findings(&db, sql);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(findings[0].severity, Severity::Warning);
            assert_eq!(
                findings[0].message,
                format!("NOT IN (SELECT …) returns no rows when {side}")
            );
            assert_eq!(
                findings[0].help.as_deref(),
                Some("use NOT EXISTS (SELECT 1 FROM … WHERE …), or make both sides NOT NULL")
            );
        }
        let sql = "SELECT id FROM users WHERE email NOT IN (SELECT title FROM posts)";
        let findings = not_in_findings(&db, sql);
        assert_eq!(
            &sql[findings[0].span.clone()],
            "email NOT IN (SELECT title FROM posts)"
        );
    }

    #[test]
    fn not_in_nullable_accepts_proven_non_null_sides() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE id NOT IN (SELECT id FROM posts)",
            "SELECT id FROM users WHERE id NOT IN (SELECT author_id FROM posts WHERE author_id IS NOT NULL)",
            "SELECT id FROM users WHERE id::text NOT IN (SELECT title FROM posts WHERE title IS NOT NULL)",
        ] {
            let findings = not_in_findings(&db, sql);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // The left side may still be NULL.
        let sql = "SELECT id FROM users WHERE email NOT IN (SELECT 'x')";
        assert_eq!(
            not_in_findings(&db, sql)[0].message,
            "NOT IN (SELECT …) returns no rows when the left side is NULL"
        );
        // A NOT NULL column on each side is enough for the row form.
        let sql =
            "SELECT id FROM users WHERE (id, id::text) NOT IN (SELECT id, id::text FROM users)";
        assert!(not_in_findings(&db, sql).is_empty(), "{sql}");
        let sql = "SELECT id FROM users WHERE (id, email) NOT IN (SELECT id, title FROM posts)";
        assert_eq!(not_in_findings(&db, sql).len(), 1, "{sql}");
    }

    #[test]
    fn not_in_nullable_ignores_lists_and_other_operators() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE id NOT IN (1, NULL)",
            "SELECT id FROM users WHERE id NOT IN (1, 2)",
            "SELECT id FROM users WHERE id = ANY (SELECT author_id FROM posts)",
            "SELECT id FROM users WHERE id NOT IN (SELECT id FROM posts)",
        ] {
            assert!(
                not_in_findings(&db, sql).is_empty(),
                "{sql}: {:?}",
                not_in_findings(&db, sql)
            );
        }
    }

    #[test]
    fn query_view_collects_sublink_facts_without_leaking_from_analyze() {
        let db = catalog();
        // A plain analyze records nothing: the next view still finds its own
        // fact, and a view without a sublink finds none.
        db.analyze("SELECT id FROM users WHERE email NOT IN (SELECT title FROM posts)")
            .unwrap();
        let view = db
            .query_view("SELECT id FROM users WHERE email NOT IN (SELECT title FROM posts)")
            .unwrap();
        let location = view
            .tree
            .nodes()
            .into_iter()
            .find_map(|(node, _)| match node {
                typedpg_pg_query::NodeRef::SubLink(sub) => Some(sub.location),
                _ => None,
            })
            .unwrap();
        assert!(view.sublink_nullability(location).is_some());
        let view = db.query_view("SELECT id FROM users").unwrap();
        assert!(view.sublink_nullability(0).is_none());
    }

    // ── migration rules ────────────────────────────────────────────────────

    const BASE: &str = r#"
        CREATE TABLE users (
            id integer PRIMARY KEY,
            name text NOT NULL,
            email text,
            v varchar(10),
            b varbit(10),
            n numeric(10,2),
            txt text,
            i integer
        );
        CREATE TABLE posts (id integer PRIMARY KEY, author_id integer, title text);
        CREATE TABLE comments (id integer PRIMARY KEY, post_id integer);
        CREATE TABLE parents (a integer, b integer, PRIMARY KEY (a, b));
        CREATE TABLE pairs (id integer PRIMARY KEY, a integer, b integer, c integer);
        CREATE TABLE constrained (id integer PRIMARY KEY, x integer);
    "#;

    fn migrations(
        files: &[(&str, &str, MigrationLint)],
        use_transaction: bool,
    ) -> Vec<MigrationFinding> {
        let files: Vec<MigrationFile> = files
            .iter()
            .map(|(path, sql, lint)| MigrationFile {
                path: (*path).to_owned(),
                sql: (*sql).to_owned(),
                lint: *lint,
            })
            .collect();
        lint_migrations(&files, use_transaction, &LintOptions::default())
            .unwrap()
            .findings
    }

    fn second(sql: &str) -> Vec<MigrationFinding> {
        migrations(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                ("002_change.sql", sql, MigrationLint::All),
            ],
            true,
        )
    }

    fn only(findings: &[MigrationFinding], rule: Rule) -> Vec<MigrationFinding> {
        findings
            .iter()
            .filter(|finding| finding.finding.rule == rule)
            .cloned()
            .collect()
    }

    #[test]
    fn migration_rules_ignore_tables_created_in_the_same_file() {
        let findings = migrations(
            &[(
                "001_all.sql",
                &format!(
                    "{BASE}
                     CREATE INDEX users_email_idx ON users (email);
                     ALTER TABLE users ALTER COLUMN i TYPE bigint;
                     ALTER TABLE users ALTER COLUMN email SET NOT NULL;
                     ALTER TABLE posts ADD CONSTRAINT posts_author_fk
                         FOREIGN KEY (author_id) REFERENCES users (id);"
                ),
                MigrationLint::All,
            )],
            true,
        );
        for rule in [
            Rule::CreateIndexBlocking,
            Rule::ConstraintValidating,
            Rule::SetNotNullScan,
            Rule::TableRewrite,
        ] {
            assert!(only(&findings, rule).is_empty(), "{rule}: {findings:?}");
        }
    }

    #[test]
    fn create_index_blocking_reports_only_non_concurrent_indexes() {
        let findings = second("CREATE INDEX users_email_idx ON users (email);");
        assert_eq!(only(&findings, Rule::CreateIndexBlocking).len(), 1);
        assert_eq!(
            only(&findings, Rule::CreateIndexBlocking)[0]
                .finding
                .message,
            "CREATE INDEX without CONCURRENTLY blocks writes on public.users for the whole build"
        );
        let findings = migrations(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_change.sql",
                    "-- no-transaction\nCREATE INDEX CONCURRENTLY users_email_idx ON users (email);",
                    MigrationLint::All,
                ),
            ],
            true,
        );
        assert!(only(&findings, Rule::CreateIndexBlocking).is_empty());
        let findings = migrations(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_change.sql",
                    "CREATE INDEX CONCURRENTLY users_email_idx ON users (email);",
                    MigrationLint::All,
                ),
            ],
            false,
        );
        assert!(only(&findings, Rule::CreateIndexBlocking).is_empty());
    }

    #[test]
    fn constraint_validating_reports_foreign_keys_and_checks() {
        for sql in [
            "ALTER TABLE posts ADD CONSTRAINT posts_author_fk FOREIGN KEY (author_id) REFERENCES users (id);",
            "ALTER TABLE posts ADD CONSTRAINT posts_title_check CHECK (title <> '');",
            "ALTER TABLE posts ADD COLUMN user_id integer REFERENCES users (id);",
            "ALTER TABLE posts ADD COLUMN title_len integer CHECK (title_len >= 0);",
        ] {
            let findings = second(sql);
            assert_eq!(
                only(&findings, Rule::ConstraintValidating).len(),
                1,
                "{sql}: {findings:?}"
            );
        }
        for sql in [
            "ALTER TABLE posts ADD CONSTRAINT posts_author_fk FOREIGN KEY (author_id) REFERENCES users (id) NOT VALID;",
            "ALTER TABLE posts ADD CONSTRAINT posts_title_check CHECK (title <> '') NOT VALID;",
        ] {
            let findings = second(sql);
            assert!(
                only(&findings, Rule::ConstraintValidating).is_empty(),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn set_not_null_scan_accepts_a_validated_check() {
        let findings = second("ALTER TABLE users ALTER COLUMN email SET NOT NULL;");
        assert_eq!(only(&findings, Rule::SetNotNullScan).len(), 1);
        for sql in [
            "ALTER TABLE users ADD CONSTRAINT users_email_nn CHECK (email IS NOT NULL);
             ALTER TABLE users ALTER COLUMN email SET NOT NULL;",
            "ALTER TABLE users ADD CONSTRAINT users_email_nn CHECK (email IS NOT NULL) NOT VALID;
             ALTER TABLE users VALIDATE CONSTRAINT users_email_nn;
             ALTER TABLE users ALTER COLUMN email SET NOT NULL;",
        ] {
            let findings = second(sql);
            assert!(
                only(&findings, Rule::SetNotNullScan).is_empty(),
                "{sql}: {findings:?}"
            );
        }
        // An unvalidated check proves nothing.
        let findings = second(
            "ALTER TABLE users ADD CONSTRAINT users_email_nn CHECK (email IS NOT NULL) NOT VALID;
             ALTER TABLE users ALTER COLUMN email SET NOT NULL;",
        );
        assert_eq!(only(&findings, Rule::SetNotNullScan).len(), 1);
    }

    #[test]
    fn table_rewrite_reports_rewriting_changes() {
        for sql in [
            "ALTER TABLE users ALTER COLUMN i TYPE bigint;",
            "ALTER TABLE users ALTER COLUMN v TYPE varchar(10) USING v::varchar(10);",
            "ALTER TABLE users ADD COLUMN created_at timestamptz DEFAULT clock_timestamp();",
            "ALTER TABLE users ADD COLUMN generated integer GENERATED ALWAYS AS (i * 2) STORED;",
        ] {
            let findings = second(sql);
            assert_eq!(
                only(&findings, Rule::TableRewrite).len(),
                1,
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn table_rewrite_ignores_no_rewrite_changes() {
        for sql in [
            "ALTER TABLE users ALTER COLUMN v TYPE varchar(20);",
            "ALTER TABLE users ALTER COLUMN v TYPE varchar;",
            "ALTER TABLE users ALTER COLUMN v TYPE text;",
            "ALTER TABLE users ALTER COLUMN b TYPE varbit(20);",
            "ALTER TABLE users ALTER COLUMN n TYPE numeric(12,2);",
            "ALTER TABLE users ALTER COLUMN txt TYPE varchar;",
            "ALTER TABLE users ADD COLUMN counter integer DEFAULT 0;",
            "ALTER TABLE users ADD COLUMN seen_at timestamptz DEFAULT now();",
        ] {
            let findings = second(sql);
            assert!(
                only(&findings, Rule::TableRewrite).is_empty(),
                "{sql}: {findings:?}"
            );
        }
        // A narrower numeric precision does rewrite.
        let findings = second("ALTER TABLE users ALTER COLUMN n TYPE numeric(8,2);");
        assert_eq!(only(&findings, Rule::TableRewrite).len(), 1);
    }

    #[test]
    fn fk_without_index_matches_leading_index_columns_as_a_set() {
        let no_index = second(
            "ALTER TABLE posts ADD CONSTRAINT posts_author_fk FOREIGN KEY (author_id) REFERENCES users (id);",
        );
        assert_eq!(only(&no_index, Rule::FkWithoutIndex).len(), 1);
        assert_eq!(
            only(&no_index, Rule::FkWithoutIndex)[0].finding.message,
            "foreign key posts_author_fk on public.posts (author_id) has no index"
        );
        assert_eq!(
            only(&no_index, Rule::FkWithoutIndex)[0]
                .finding
                .help
                .as_deref(),
            Some("CREATE INDEX ON public.posts (author_id)")
        );
        for index in [
            "CREATE INDEX pairs_ab_idx ON pairs (a, b);",
            "CREATE INDEX pairs_ba_idx ON pairs (b, a);",
            "CREATE INDEX pairs_abc_idx ON pairs (a, b, c);",
        ] {
            let findings = second(&format!(
                "ALTER TABLE pairs ADD CONSTRAINT pairs_fk FOREIGN KEY (a, b) REFERENCES parents (a, b);
                 {index}"
            ));
            assert!(
                only(&findings, Rule::FkWithoutIndex).is_empty(),
                "{index}: {findings:?}"
            );
        }
        // A single-column key with a trailing column counts; one where the
        // key is only a trailing column does not.
        let trailing = second(
            "ALTER TABLE posts ADD CONSTRAINT posts_author_fk FOREIGN KEY (author_id) REFERENCES users (id);
             CREATE INDEX posts_author_id_idx ON posts (author_id, id);",
        );
        assert!(
            only(&trailing, Rule::FkWithoutIndex).is_empty(),
            "{trailing:?}"
        );
        let not_leading = second(
            "ALTER TABLE posts ADD CONSTRAINT posts_author_fk FOREIGN KEY (author_id) REFERENCES users (id);
             CREATE INDEX posts_id_author_idx ON posts (id, author_id);",
        );
        assert_eq!(only(&not_leading, Rule::FkWithoutIndex).len(), 1);

        // A partial index or an expression position doesn't count.
        for index in [
            "CREATE INDEX pairs_ab_partial ON pairs (a, b) WHERE a > 0;",
            "CREATE INDEX pairs_expr ON pairs ((a + 0), b);",
        ] {
            let findings = second(&format!(
                "ALTER TABLE pairs ADD CONSTRAINT pairs_fk FOREIGN KEY (a, b) REFERENCES parents (a, b);
                 {index}"
            ));
            assert_eq!(
                only(&findings, Rule::FkWithoutIndex).len(),
                1,
                "{index}: {findings:?}"
            );
        }
    }

    #[test]
    fn fk_without_index_accepts_not_null_partial_indexes() {
        // A partial index whose predicate only requires the referencing
        // columns to be NOT NULL serves the foreign-key check: its equality
        // lookup implies exactly that.
        for index in [
            "CREATE INDEX posts_author_partial ON posts (author_id) WHERE author_id IS NOT NULL;",
            "CREATE INDEX posts_author_partial ON posts (author_id) WHERE (author_id) IS NOT NULL;",
        ] {
            let findings = second(&format!(
                "ALTER TABLE posts ADD CONSTRAINT posts_author_fk FOREIGN KEY (author_id) REFERENCES users (id);
                 {index}"
            ));
            assert!(
                only(&findings, Rule::FkWithoutIndex).is_empty(),
                "{index}: {findings:?}"
            );
        }
        // A composite key's predicate can test each column, with nested ANDs
        // and parentheses.
        for index in [
            "CREATE INDEX pairs_ab_partial ON pairs (a, b) WHERE a IS NOT NULL AND b IS NOT NULL;",
            "CREATE INDEX pairs_ab_partial ON pairs (a, b) WHERE (a IS NOT NULL AND (b IS NOT NULL));",
            "CREATE INDEX pairs_ab_partial ON pairs (a, b) WHERE pairs.a IS NOT NULL AND pairs.b IS NOT NULL;",
        ] {
            let findings = second(&format!(
                "ALTER TABLE pairs ADD CONSTRAINT pairs_fk FOREIGN KEY (a, b) REFERENCES parents (a, b);
                 {index}"
            ));
            assert!(
                only(&findings, Rule::FkWithoutIndex).is_empty(),
                "{index}: {findings:?}"
            );
        }
        // Anything else does not count.
        for index in [
            "CREATE INDEX posts_author_partial ON posts (author_id) WHERE id IS NOT NULL;",
            "CREATE INDEX posts_author_partial ON posts (author_id) WHERE author_id IS NULL;",
            "CREATE INDEX posts_author_partial ON posts (author_id) WHERE title = 'x';",
            "CREATE INDEX posts_author_partial ON posts (author_id) WHERE author_id = 1;",
            "CREATE INDEX posts_author_partial ON posts (author_id) WHERE NOT author_id IS NULL;",
            "CREATE INDEX pairs_ab_partial ON pairs (a, b) WHERE a IS NOT NULL OR b IS NOT NULL;",
            "CREATE INDEX pairs_ab_partial ON pairs (a, b) WHERE (a, b) IS NOT NULL;",
        ] {
            let (table, key) = if index.contains("pairs") {
                (
                    "pairs",
                    "ALTER TABLE pairs ADD CONSTRAINT pairs_fk FOREIGN KEY (a, b) REFERENCES parents (a, b);",
                )
            } else {
                (
                    "posts",
                    "ALTER TABLE posts ADD CONSTRAINT posts_author_fk FOREIGN KEY (author_id) REFERENCES users (id);",
                )
            };
            let findings = second(&format!("{key}\n{index}"));
            assert_eq!(
                only(&findings, Rule::FkWithoutIndex).len(),
                1,
                "{table} {index}: {findings:?}"
            );
        }
    }

    #[test]
    fn redundant_index_prefix_exact_and_unique_cases() {
        let prefix = second(
            "CREATE INDEX idx_users_email ON users (email);
             CREATE INDEX idx_users_email_name ON users (email, name);",
        );
        assert_eq!(only(&prefix, Rule::RedundantIndex).len(), 1);
        assert_eq!(
            only(&prefix, Rule::RedundantIndex)[0].finding.message,
            "index idx_users_email is redundant with idx_users_email_name"
        );
        assert_eq!(
            only(&prefix, Rule::RedundantIndex)[0].finding.severity,
            Severity::Info
        );

        let exact = second(
            "CREATE INDEX idx_users_email ON users (email);
             CREATE INDEX idx_users_email_two ON users (email);",
        );
        assert_eq!(only(&exact, Rule::RedundantIndex).len(), 1);
        assert_eq!(
            only(&exact, Rule::RedundantIndex)[0].finding.message,
            "index idx_users_email_two is redundant with idx_users_email"
        );

        let unique = second(
            "CREATE UNIQUE INDEX idx_users_email ON users (email);
             CREATE UNIQUE INDEX idx_users_email_two ON users (email);",
        );
        assert_eq!(only(&unique, Rule::RedundantIndex).len(), 1);
        assert_eq!(
            only(&unique, Rule::RedundantIndex)[0].finding.message,
            "index idx_users_email_two is redundant with idx_users_email"
        );
        // A non-unique index is covered by a broader unique one; a unique
        // index is not covered by a non-unique exact one.
        let unique_prefix = second(
            "CREATE INDEX idx_users_email ON users (email);
             CREATE UNIQUE INDEX idx_users_email_name ON users (email, name);",
        );
        assert_eq!(only(&unique_prefix, Rule::RedundantIndex).len(), 1);
        let unique_not_covered = second(
            "CREATE INDEX idx_users_email_name ON users (email, name);
             CREATE UNIQUE INDEX idx_users_email_unique ON users (email);",
        );
        assert!(only(&unique_not_covered, Rule::RedundantIndex).is_empty());

        // A different operator class makes the indexes different.
        let opclass = second(
            "CREATE INDEX idx_users_txt ON users (txt);
             CREATE INDEX idx_users_txt_pattern ON users (txt text_pattern_ops);",
        );
        assert!(only(&opclass, Rule::RedundantIndex).is_empty());

        // An index backing a constraint is only reported as an exact
        // duplicate; a plain index with its columns is still redundant.
        let constraint = second("CREATE INDEX constrained_id_idx ON constrained (id);");
        assert_eq!(only(&constraint, Rule::RedundantIndex).len(), 1);
        assert_eq!(
            only(&constraint, Rule::RedundantIndex)[0].finding.message,
            "index constrained_id_idx is redundant with constrained_pkey"
        );
    }

    #[test]
    fn redundant_index_respects_include_columns_and_sort_order() {
        // A covering index serves index-only scans the plain one can't: it
        // is not redundant, while the plain one is.
        let covering = second(
            "CREATE INDEX idx_users_email ON users (email);
             CREATE INDEX idx_users_email_cover ON users (email) INCLUDE (name);",
        );
        let found = only(&covering, Rule::RedundantIndex);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].finding.message,
            "index idx_users_email is redundant with idx_users_email_cover"
        );
        let two_covering = second(
            "CREATE INDEX idx_a ON users (email) INCLUDE (name);
             CREATE INDEX idx_b ON users (email) INCLUDE (txt);",
        );
        assert!(only(&two_covering, Rule::RedundantIndex).is_empty());

        // A different order serves different ORDER BYs.
        for sql in [
            "CREATE INDEX idx_a ON users (email, name);
             CREATE INDEX idx_b ON users (email, name DESC);",
            "CREATE INDEX idx_a ON users (email);
             CREATE INDEX idx_b ON users (email NULLS FIRST, name);",
        ] {
            assert!(only(&second(sql), Rule::RedundantIndex).is_empty(), "{sql}");
        }
        // The defaults spelled out are the same order.
        let spelled = second(
            "CREATE INDEX idx_a ON users (email DESC NULLS FIRST);
             CREATE INDEX idx_b ON users (email DESC, name);",
        );
        assert_eq!(only(&spelled, Rule::RedundantIndex).len(), 1);
    }

    #[test]
    fn set_not_null_scan_ignores_a_column_already_not_null() {
        let findings = second("ALTER TABLE users ALTER COLUMN name SET NOT NULL;");
        assert!(
            only(&findings, Rule::SetNotNullScan).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn fk_without_index_needs_an_index_that_serves_equality() {
        for (index, indexed) in [
            ("CREATE INDEX ON posts USING hash (author_id);", true),
            ("CREATE INDEX ON posts USING brin (author_id);", false),
        ] {
            let findings = second(&format!(
                "{index}
                 ALTER TABLE posts ADD CONSTRAINT posts_author_fk
                     FOREIGN KEY (author_id) REFERENCES users (id) NOT VALID;"
            ));
            assert_eq!(
                only(&findings, Rule::FkWithoutIndex).is_empty(),
                indexed,
                "{index}: {findings:?}"
            );
        }
    }

    #[test]
    fn adding_an_index_backed_constraint_blocks_writes() {
        for sql in [
            "ALTER TABLE posts ADD CONSTRAINT posts_title_key UNIQUE (title);",
            "ALTER TABLE comments ADD COLUMN slug text UNIQUE;",
            "ALTER TABLE pairs ADD CONSTRAINT pairs_ab_key UNIQUE (a, b);",
        ] {
            let found = only(&second(sql), Rule::CreateIndexBlocking);
            assert_eq!(found.len(), 1, "{sql}: {found:?}");
            assert!(
                found[0]
                    .finding
                    .message
                    .starts_with("adding the constraint builds its index"),
                "{found:?}"
            );
        }
        // Adopting an index built concurrently builds nothing.
        let findings = migrations(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_index.sql",
                    "-- no-transaction\nCREATE UNIQUE INDEX CONCURRENTLY posts_title_key ON posts (title);",
                    MigrationLint::All,
                ),
                (
                    "003_constraint.sql",
                    "ALTER TABLE posts ADD CONSTRAINT posts_title_key UNIQUE USING INDEX posts_title_key;",
                    MigrationLint::All,
                ),
            ],
            true,
        );
        assert!(
            only(&findings, Rule::CreateIndexBlocking).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn migration_allow_comments_suppress_per_statement() {
        for comment in [
            "-- typedpg-lint: allow(create_index_blocking)",
            "/* typedpg-lint: allow(create_index_blocking) */",
        ] {
            let findings = second(&format!(
                "{comment}\nCREATE INDEX users_email_idx ON users (email);"
            ));
            assert!(
                only(&findings, Rule::CreateIndexBlocking).is_empty(),
                "{comment}"
            );
        }
        // The comment applies to the statement it leads into, not the next.
        let text = "-- typedpg-lint: allow(create_index_blocking)\nCREATE INDEX users_name_idx ON users (name);\n\
                    CREATE INDEX users_email_idx ON users (email);";
        let findings = second(text);
        let blocking = only(&findings, Rule::CreateIndexBlocking);
        assert_eq!(blocking.len(), 1, "{findings:?}");
        assert_eq!(
            &text[blocking[0].finding.span.clone()],
            "CREATE INDEX users_email_idx ON users (email)"
        );
    }

    #[test]
    fn migration_allow_comments_are_validated() {
        let findings = second(
            "-- typedpg-lint: allow(no_such_rule)\nCREATE INDEX users_email_idx ON users (email);",
        );
        let invalid = only(&findings, Rule::InvalidAllow);
        assert_eq!(invalid.len(), 1, "{findings:?}");
        assert_eq!(invalid[0].finding.severity, Severity::Error);
        assert!(invalid[0].finding.message.contains("unknown lint rule"));
        assert_eq!(only(&findings, Rule::CreateIndexBlocking).len(), 1);

        let findings = second(
            "-- typedpg-lint: alow(create_index_blocking)\nCREATE INDEX users_email_idx ON users (email);",
        );
        let invalid = only(&findings, Rule::InvalidAllow);
        assert_eq!(invalid.len(), 1, "{findings:?}");
        assert!(
            invalid[0]
                .finding
                .message
                .starts_with("malformed typedpg-lint comment")
        );
    }

    #[test]
    fn none_files_report_nothing() {
        let findings = migrations(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_extra.sql",
                    "-- typedpg-lint: allow(no_such_rule)
                     CREATE INDEX posts_author_idx ON posts (author_id);
                     ALTER TABLE posts ADD CONSTRAINT posts_author_fk
                         FOREIGN KEY (author_id) REFERENCES users (id);",
                    MigrationLint::None,
                ),
            ],
            true,
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn schema_only_files_skip_statement_rules_and_report_schema_rules() {
        let findings = migrations(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_unchanged.sql",
                    "CREATE TABLE books (
                         id integer PRIMARY KEY,
                         author_id integer REFERENCES users (id)
                     );
                     CREATE INDEX users_email_idx ON users (email);",
                    MigrationLint::SchemaOnly,
                ),
            ],
            true,
        );
        assert!(
            only(&findings, Rule::CreateIndexBlocking).is_empty(),
            "{findings:?}"
        );
        let fk = only(&findings, Rule::FkWithoutIndex);
        assert_eq!(fk.len(), 1, "{findings:?}");
        assert_eq!(fk[0].file, 1);
        assert_eq!(
            fk[0].finding.help.as_deref(),
            Some(
                "CREATE INDEX CONCURRENTLY ON public.books (author_id), in a new migration whose \
                 first line is -- no-transaction"
            )
        );
    }

    #[test]
    fn schema_only_redundant_indexes_point_at_a_new_migration() {
        let findings = migrations(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_unchanged.sql",
                    "CREATE INDEX users_email_idx ON users (email);
                     CREATE INDEX users_email_idx_two ON users (email);",
                    MigrationLint::SchemaOnly,
                ),
            ],
            true,
        );
        let redundant = only(&findings, Rule::RedundantIndex);
        assert_eq!(redundant.len(), 1, "{findings:?}");
        assert_eq!(redundant[0].file, 1);
        assert_eq!(
            redundant[0].finding.help.as_deref(),
            Some(
                "DROP INDEX CONCURRENTLY users_email_idx_two, in a new migration whose first line \
                 is -- no-transaction"
            )
        );
    }

    #[test]
    fn schema_only_files_honor_allow_comments() {
        let findings = migrations(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_unchanged.sql",
                    "-- typedpg-lint: allow(fk_without_index)
                     CREATE TABLE books (
                         id integer PRIMARY KEY,
                         author_id integer REFERENCES users (id)
                     );
                     -- typedpg-lint: allow(create_index_blocking)
                     CREATE INDEX users_email_idx ON users (email);",
                    MigrationLint::SchemaOnly,
                ),
            ],
            true,
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn schema_only_files_still_validate_allow_comments() {
        let findings = migrations(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_unchanged.sql",
                    "-- typedpg-lint: allow(no_such_rule)
                     CREATE INDEX users_email_idx ON users (email);",
                    MigrationLint::SchemaOnly,
                ),
            ],
            true,
        );
        let invalid = only(&findings, Rule::InvalidAllow);
        assert_eq!(invalid.len(), 1, "{findings:?}");
        assert_eq!(invalid[0].file, 1);
        assert!(invalid[0].finding.message.contains("unknown lint rule"));
    }

    #[test]
    fn migration_findings_are_trimmed_statements_in_their_file() {
        // This test pins `create_index_blocking`'s span; the newer
        // `missing_lock_timeout` also reports a non-concurrent CREATE INDEX,
        // so keep it out of this count.
        let text = "-- typedpg-lint: allow(missing_lock_timeout)\n  CREATE INDEX users_email_idx ON users (email);  ";
        let findings = second(text);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            &text[findings[0].finding.span.clone()],
            "CREATE INDEX users_email_idx ON users (email)"
        );
    }

    #[test]
    fn migration_rule_names_and_defaults() {
        assert_rule_lists_match();
        for (rule, severity) in [
            (Rule::NotInNullable, Severity::Warning),
            (Rule::CreateIndexBlocking, Severity::Warning),
            (Rule::ConstraintValidating, Severity::Warning),
            (Rule::SetNotNullScan, Severity::Warning),
            (Rule::TableRewrite, Severity::Warning),
            (Rule::FkWithoutIndex, Severity::Warning),
            (Rule::RedundantIndex, Severity::Info),
        ] {
            assert_eq!(rule.default_severity(), severity, "{rule}");
        }
    }
}
