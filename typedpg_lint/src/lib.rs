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
//! Rendering lives here too, so the Rust and TypeScript CLIs report the
//! same lines.

mod rules;

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
    /// A malformed or unknown rule name in an allow comment. Never
    /// configurable and never suppressible: it is always an error.
    InvalidAllow,
}

impl Rule {
    /// Every configurable rule.
    pub const ALL: [Rule; 2] = [Rule::NullComparison, Rule::UnboundedWrite];

    /// The rule's configuration key (`[package.metadata.typedpg.lint]`, and
    /// the TypeScript CLI's `lint` object in camelCase).
    pub fn name(self) -> &'static str {
        match self {
            Rule::NullComparison => "null_comparison",
            Rule::UnboundedWrite => "unbounded_write",
            Rule::InvalidAllow => "invalid_allow",
        }
    }

    /// The configurable rule named `name` (see [`Rule::name`]).
    pub fn from_name(name: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|r| r.name() == name)
    }

    /// The severity the rule reports unless the configuration overrides it.
    pub fn default_severity(self) -> Severity {
        match self {
            Rule::NullComparison | Rule::UnboundedWrite | Rule::InvalidAllow => Severity::Error,
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
    /// parameters and `$..spreads` included).
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

/// The summary line:
/// `[typedpg] lint: 2 errors, 1 warning, 0 infos in 14 queries`.
pub fn summary(findings: &[Finding], queries: usize) -> String {
    let count = |severity| {
        findings
            .iter()
            .filter(|finding| finding.severity == severity)
            .count()
    };
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
    format!(
        "[typedpg] lint: {}, {}, {} in {queries}",
        plural(count(Severity::Error), "error", "errors"),
        plural(count(Severity::Warning), "warning", "warnings"),
        plural(count(Severity::Info), "info", "infos"),
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
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            let finding = &findings[0];
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
        assert_eq!(
            summary(&[finding, warning, info], 14),
            "[typedpg] lint: 1 error, 1 warning, 1 info in 14 queries"
        );
        assert_eq!(
            summary(&[], 1),
            "[typedpg] lint: 0 errors, 0 warnings, 0 infos in 1 query"
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
}
