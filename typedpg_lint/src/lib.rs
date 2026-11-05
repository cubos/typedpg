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
//! [`report`] builds and renders the one report both CLIs print: the same
//! lines in the human format, as one JSON object, or as SARIF.

mod migrations;
pub mod report;
mod rules;

pub use migrations::{LintedMigrations, MigrationFile, MigrationFinding, lint_migrations};

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
    /// A malformed or unknown rule name in an allow comment. Never
    /// configurable and never suppressible: it is always an error.
    InvalidAllow,
}

impl Rule {
    /// Every configurable rule.
    pub const ALL: [Rule; 9] = [
        Rule::NullComparison,
        Rule::UnboundedWrite,
        Rule::NotInNullable,
        Rule::CreateIndexBlocking,
        Rule::ConstraintValidating,
        Rule::SetNotNullScan,
        Rule::TableRewrite,
        Rule::FkWithoutIndex,
        Rule::RedundantIndex,
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
            Rule::InvalidAllow => "invalid_allow",
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
            Rule::InvalidAllow => "a malformed or unknown rule name in a typedpg-lint comment",
        }
    }

    /// The severity the rule reports unless the configuration overrides it.
    pub fn default_severity(self) -> Severity {
        match self {
            Rule::NullComparison | Rule::UnboundedWrite | Rule::InvalidAllow => Severity::Error,
            Rule::NotInNullable
            | Rule::CreateIndexBlocking
            | Rule::ConstraintValidating
            | Rule::SetNotNullScan
            | Rule::TableRewrite
            | Rule::FkWithoutIndex => Severity::Warning,
            Rule::RedundantIndex => Severity::Info,
        }
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
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
                .map(|rule| (rule, Some(rule.default_severity())))
                .collect(),
            fail_on: Severity::Error,
        }
    }
}

impl LintOptions {
    /// The options `[package.metadata.typedpg.lint]` (or the TypeScript
    /// config's `lint` object) configures: rules without an override keep
    /// their [`Rule::default_severity`], and `fail_on` defaults to error.
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
    };
    for rule in Rule::ALL {
        let Some(severity) = options.level(rule) else {
            continue;
        };
        if suppressed.contains(&rule) {
            continue;
        }
        for raw in rules::findings(rule, &ctx) {
            findings.push(Finding {
                rule,
                severity,
                span: raw.span,
                message: raw.message,
                help: raw.help,
            });
        }
    }
    for finding in &mut findings {
        finding.span = view.original_span(finding.span.clone());
    }
    // A deterministic order within a query: by position, then rule.
    findings.sort_by(|a, b| {
        (a.span.start, a.span.end, a.rule).cmp(&(b.span.start, b.span.end, b.rule))
    });
    Ok(findings)
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
                    });
                }
            }
        }
    }
    (suppressed, errors)
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

/// The summary line [`report::Report::summary`] renders:
/// `[typedpg] lint: 2 errors, 1 warning, 0 infos in 14 queries and 3 migrations`.
pub(crate) fn summary_line(
    errors: usize,
    warnings: usize,
    infos: usize,
    queries: usize,
    migrations: usize,
) -> String {
    let plural = |n: usize, word: &str, words: &str| {
        if n == 1 {
            format!("{n} {word}")
        } else {
            format!("{n} {words}")
        }
    };
    let queries = if queries == 1 {
        "1 query".to_owned()
    } else {
        format!("{queries} queries")
    };
    let migrations = if migrations == 1 {
        "1 migration".to_owned()
    } else {
        format!("{migrations} migrations")
    };
    format!(
        "[typedpg] lint: {}, {}, {} in {queries} and {migrations}",
        plural(errors, "error", "errors"),
        plural(warnings, "warning", "warnings"),
        plural(infos, "info", "infos"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use typedpg_analyzer::PgCatalog;
    use typedpg_core::config::{LINT_RULES, LintLevel, LintSeverity};

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

    #[test]
    fn config_rule_names_match_the_lint_crate() {
        let names: Vec<&str> = Rule::ALL.iter().map(|rule| rule.name()).collect();
        assert_eq!(names, LINT_RULES);
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
        let findings = lint(&db, sql);
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
            failures: Vec::new(),
            queries: 14,
            migrations: 3,
        };
        assert_eq!(
            report.summary(),
            "[typedpg] lint: 1 error, 1 warning, 1 info in 14 queries and 3 migrations"
        );
        let report = report::Report {
            entries: Vec::new(),
            failures: Vec::new(),
            queries: 1,
            migrations: 1,
        };
        assert_eq!(
            report.summary(),
            "[typedpg] lint: 0 errors, 0 warnings, 0 infos in 1 query and 1 migration"
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

    fn migrations(files: &[(&str, &str, bool)], use_transaction: bool) -> Vec<MigrationFinding> {
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
            &[("001_base.sql", BASE, true), ("002_change.sql", sql, true)],
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
                true,
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
                ("001_base.sql", BASE, true),
                (
                    "002_change.sql",
                    "-- no-transaction\nCREATE INDEX CONCURRENTLY users_email_idx ON users (email);",
                    true,
                ),
            ],
            true,
        );
        assert!(only(&findings, Rule::CreateIndexBlocking).is_empty());
        let findings = migrations(
            &[
                ("001_base.sql", BASE, true),
                (
                    "002_change.sql",
                    "CREATE INDEX CONCURRENTLY users_email_idx ON users (email);",
                    true,
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
                ("001_base.sql", BASE, true),
                (
                    "002_index.sql",
                    "-- no-transaction\nCREATE UNIQUE INDEX CONCURRENTLY posts_title_key ON posts (title);",
                    true,
                ),
                (
                    "003_constraint.sql",
                    "ALTER TABLE posts ADD CONSTRAINT posts_title_key UNIQUE USING INDEX posts_title_key;",
                    true,
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
    fn lint_false_files_are_never_reported() {
        let findings = migrations(
            &[
                ("001_base.sql", BASE, true),
                (
                    "002_extra.sql",
                    "-- typedpg-lint: allow(no_such_rule)
                     CREATE INDEX posts_author_idx ON posts (author_id);
                     ALTER TABLE posts ADD CONSTRAINT posts_author_fk
                         FOREIGN KEY (author_id) REFERENCES users (id);",
                    false,
                ),
            ],
            true,
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn migration_findings_are_trimmed_statements_in_their_file() {
        let text = "-- lead\n  CREATE INDEX users_email_idx ON users (email);  ";
        let findings = second(text);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            &text[findings[0].finding.span.clone()],
            "CREATE INDEX users_email_idx ON users (email)"
        );
    }

    #[test]
    fn migration_rule_names_and_defaults() {
        assert_eq!(
            Rule::ALL.iter().map(|rule| rule.name()).collect::<Vec<_>>(),
            LINT_RULES
        );
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
