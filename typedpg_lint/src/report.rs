//! The lint report both CLIs build and render.
//!
//! A report is one list of findings, one list of analysis failures, the
//! counts [`Report::summary`] shows, the objects each database's migrations
//! created, and how long the run took. [`Format::Human`] renders the failures
//! first (they block the build), then every finding in its file, as the
//! `typedpg check` box with file line numbers; [`Format::Json`] and
//! [`Format::Sarif`] put the failures in the document too.

use std::collections::BTreeMap;
use std::fmt;
use std::io::IsTerminal;
use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;
use typedpg_analyzer::UserObjectCounts;

use crate::{Finding, LintOptions, Rule, Severity};

/// A 1-based line and a 1-based column, counted in Unicode code points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

/// The position of the character at byte `byte_offset` of `text`.
///
/// Columns count Unicode code points, not bytes: the text before a site may
/// contain non-ASCII characters (an accented identifier in a comment, say)
/// and editors count them as one column each. An offset past the end points
/// at the end; one inside a character rounds down to its start.
pub fn position(text: &str, byte_offset: usize) -> Position {
    let mut offset = byte_offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let mut line = 1u32;
    let mut line_start = 0usize;
    for (i, byte) in text.as_bytes()[..offset].iter().enumerate() {
        if *byte == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    Position {
        line,
        column: text[line_start..offset].chars().count() as u32 + 1,
    }
}

/// A finding placed in its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file's path as displayed: relative to the project root, with
    /// forward slashes.
    pub path: String,
    /// Where the finding starts.
    pub start: Position,
    /// Where it ends (exclusive).
    pub end: Position,
    pub finding: Finding,
}

/// An analysis or migration error: the run failed before it could report
/// findings for something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub path: String,
    /// The position of the error, when it has one.
    pub start: Option<Position>,
    pub message: String,
}

impl Failure {
    /// The one-line form: `path:line:column: error: message`, or
    /// `path: error: message` when the failure has no position. The human
    /// report renders the boxed form instead.
    pub fn render(&self) -> String {
        match self.start {
            Some(position) => format!(
                "{}:{}:{}: error: {}",
                self.path, position.line, position.column, self.message
            ),
            None => format!("{}: error: {}", self.path, self.message),
        }
    }
}

/// Everything a lint run found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub entries: Vec<Entry>,
    pub failures: Vec<Failure>,
    /// How many statements were analyzed: `sql!` sites, `.sql` queries and
    /// `copy_in!` targets.
    pub queries: usize,
    /// How many migration files were applied.
    pub migrations: usize,
    /// The text of each file the human renderer shows a finding in, keyed
    /// by the path the entry shows.
    pub sources: BTreeMap<String, String>,
    /// Per database (named `default` when there is only one), how many
    /// objects its migrations created.
    pub schemas: Vec<(String, UserObjectCounts)>,
    /// How long the run took, for the summary block.
    pub elapsed: Duration,
}

impl Report {
    /// Read, under `root`, the text of every file a failure is in that
    /// [`Self::sources`] doesn't hold yet, so the human report can show the
    /// failing line.
    pub fn load_failure_sources(&mut self, root: &std::path::Path) {
        for failure in &self.failures {
            if failure.start.is_none() || self.sources.contains_key(&failure.path) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(root.join(&failure.path)) {
                self.sources.insert(failure.path.clone(), text);
            }
        }
    }

    /// Sort the failures and findings by path, position and rule.
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| {
            (a.path.as_str(), a.start, a.finding.rule).cmp(&(
                b.path.as_str(),
                b.start,
                b.finding.rule,
            ))
        });
        self.failures
            .sort_by(|a, b| (&a.path, a.start, &a.message).cmp(&(&b.path, b.start, &b.message)));
    }

    /// Whether the run fails: any analysis or migration failure does, and
    /// so does any finding at or above the configured `fail_on` severity.
    pub fn fails(&self, options: &LintOptions) -> bool {
        !self.failures.is_empty()
            || self
                .entries
                .iter()
                .any(|entry| entry.finding.severity >= options.fail_on())
    }

    /// The document printed to stdout in `format`, uncolored, with the
    /// default lint options.
    pub fn render(&self, format: Format) -> String {
        self.render_with(format, &LintOptions::default(), false)
    }

    /// The document printed to stdout in `format`. `colors` only affects
    /// [`Format::Human`].
    pub fn render_with(&self, format: Format, options: &LintOptions, colors: bool) -> String {
        match format {
            Format::Human => self.human(colors),
            Format::Json => self.json(),
            Format::Sarif => self.sarif(!self.fails(options)),
        }
    }

    /// The summary block, uncolored, with the default lint options.
    pub fn summary(&self) -> String {
        self.summary_with(&LintOptions::default(), false)
    }

    /// The summary block printed to stderr: the counts, the elapsed time,
    /// the schema, and whether the run passed.
    pub fn summary_with(&self, options: &LintOptions, colors: bool) -> String {
        let count = |severity| {
            self.entries
                .iter()
                .filter(|entry| entry.finding.severity == severity)
                .count()
        };
        let plural = |n: usize, one: &str, many: &str| {
            if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {many}")
            }
        };
        let counts: Vec<String> = [
            (count(Severity::Error), "error", "errors"),
            (count(Severity::Warning), "warning", "warnings"),
            (count(Severity::Info), "info", "infos"),
        ]
        .into_iter()
        .filter(|(n, _, _)| *n > 0)
        .map(|(n, one, many)| plural(n, one, many))
        .collect();
        let mut out = format!(
            "typedpg lint: {}",
            if counts.is_empty() {
                "no findings".to_owned()
            } else {
                counts.join(", ")
            }
        );
        out.push_str(&format!(
            "\n  checked {} and {} in {}",
            plural(self.queries, "query", "queries"),
            plural(self.migrations, "migration", "migrations"),
            format_duration(self.elapsed),
        ));
        if self.schemas.len() == 1 {
            out.push_str(&format!(
                "\n  schema: {}",
                schema_counts(&self.schemas[0].1)
            ));
        } else {
            for (database, counts) in &self.schemas {
                out.push_str(&format!(
                    "\n  schema ({database}): {}",
                    schema_counts(counts)
                ));
            }
        }
        if !self.failures.is_empty() {
            out.push_str(&format!(
                "\n  {} {} — the build would fail",
                paint("1;31", "✖", colors),
                if self.failures.len() == 1 {
                    "1 query doesn't compile".to_owned()
                } else {
                    format!("{} queries don't compile", self.failures.len())
                },
            ));
        }
        let failing = self
            .entries
            .iter()
            .filter(|entry| entry.finding.severity >= options.fail_on())
            .count();
        if self.fails(options) {
            let reason = if failing > 0 {
                format!(
                    "{} at or above fail_on ({})",
                    // Every severity at or above it counts: with `warning`,
                    // the errors too.
                    plural(failing, "finding", "findings"),
                    options.fail_on(),
                )
            } else {
                plural(self.failures.len(), "analysis error", "analysis errors")
            };
            out.push_str(&format!(
                "\n  {} failed: {reason}",
                paint("1;31", "✖", colors)
            ));
        } else {
            out.push_str(&format!("\n  {} passed", paint("32", "✔", colors)));
        }
        out
    }

    fn human(&self, colors: bool) -> String {
        let mut blocks: Vec<String> = self
            .failures
            .iter()
            .map(|failure| human_failure(failure, &self.sources, colors))
            .collect();
        blocks.extend(
            self.entries
                .iter()
                .map(|entry| human_entry(entry, &self.sources, colors)),
        );
        blocks.join("\n\n")
    }

    fn json(&self) -> String {
        let findings: Vec<JsonFinding<'_>> = self
            .entries
            .iter()
            .map(|entry| JsonFinding {
                path: &entry.path,
                line: entry.start.line,
                column: entry.start.column,
                end_line: entry.end.line,
                end_column: entry.end.column,
                rule: entry.finding.rule.name(),
                severity: entry.finding.severity.name(),
                message: &entry.finding.message,
                help: entry.finding.help.as_deref(),
            })
            .collect();
        let failures: Vec<JsonFailure<'_>> = self
            .failures
            .iter()
            .map(|failure| JsonFailure {
                path: &failure.path,
                line: failure.start.map(|position| position.line),
                column: failure.start.map(|position| position.column),
                message: &failure.message,
            })
            .collect();
        let count = |severity| {
            self.entries
                .iter()
                .filter(|entry| entry.finding.severity == severity)
                .count()
        };
        let schema: BTreeMap<&str, &UserObjectCounts> = self
            .schemas
            .iter()
            .map(|(name, counts)| (name.as_str(), counts))
            .collect();
        let document = JsonReport {
            findings,
            failures,
            summary: JsonSummary {
                errors: count(Severity::Error),
                warnings: count(Severity::Warning),
                infos: count(Severity::Info),
                queries: self.queries,
                migrations: self.migrations,
                failures: self.failures.len(),
                schema,
            },
        };
        serde_json::to_string_pretty(&document).expect("the report serializes")
    }

    fn sarif(&self, successful: bool) -> String {
        let mut rules = Vec::new();
        for rule in Rule::ALL.into_iter().chain([Rule::InvalidAllow]) {
            rules.push(serde_json::json!({
                "id": rule.name(),
                "shortDescription": { "text": rule.description() },
                "defaultConfiguration": { "level": sarif_level(rule.default_severity()) },
            }));
        }
        rules.push(serde_json::json!({
            "id": ANALYSIS_ERROR,
            "shortDescription": {
                "text": "the query or migration could not be analyzed"
            },
            "defaultConfiguration": { "level": "error" },
        }));

        let mut results = Vec::new();
        for entry in &self.entries {
            let mut message = entry.finding.message.clone();
            if let Some(help) = &entry.finding.help {
                message.push('\n');
                message.push_str(help);
            }
            results.push(serde_json::json!({
                "ruleId": entry.finding.rule.name(),
                "level": sarif_level(entry.finding.severity),
                "message": { "text": message },
                "locations": [location(
                    &entry.path,
                    Some(entries_region(entry)),
                )],
            }));
        }
        for failure in &self.failures {
            let region = failure.start.map(|start| {
                serde_json::json!({
                    "startLine": start.line,
                    "startColumn": start.column,
                })
            });
            results.push(serde_json::json!({
                "ruleId": ANALYSIS_ERROR,
                "level": "error",
                "message": { "text": failure.message },
                "locations": [location(&failure.path, region)],
            }));
        }

        let document = serde_json::json!({
            "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
            "version": "2.1.0",
            "runs": [{
                "tool": {
                    "driver": {
                        "name": "typedpg",
                        "informationUri": "https://github.com/cubos/typedpg",
                        "version": env!("CARGO_PKG_VERSION"),
                        "rules": rules,
                    },
                },
                "invocations": [{ "executionSuccessful": successful }],
                "columnKind": "unicodeCodePoints",
                "results": results,
            }],
        });
        serde_json::to_string_pretty(&document).expect("the report serializes")
    }
}

const ANALYSIS_ERROR: &str = "analysis_error";

/// Whether output written to a stream that is (or is not) a terminal should
/// use colors: never when `NO_COLOR` is set, always when `CLICOLOR_FORCE`
/// asks for them, and otherwise only on a terminal.
pub fn colors_enabled(is_terminal: bool) -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    match std::env::var_os("CLICOLOR_FORCE") {
        Some(value) if value != "0" => true,
        _ => is_terminal,
    }
}

/// [`colors_enabled`] for stdout.
pub fn stdout_colors() -> bool {
    colors_enabled(std::io::stdout().is_terminal())
}

/// [`colors_enabled`] for stderr.
pub fn stderr_colors() -> bool {
    colors_enabled(std::io::stderr().is_terminal())
}

fn paint(code: &str, text: impl fmt::Display, colors: bool) -> String {
    if colors {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn severity_color(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "1;31",
        Severity::Warning => "1;33",
        Severity::Info => "1;34",
    }
}

const GUTTER: &str = "2;36";

/// How wide `text` is once every tab is expanded to four spaces, up to the
/// first `chars` characters.
fn display_width(text: &str, chars: u32) -> usize {
    text.chars()
        .take(chars as usize)
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// What a failure's message says beyond its first line, taken apart: the
/// analyzer renders its own box around the SQL as analyzed, numbered from
/// the query's first line; the human report draws the file's lines instead,
/// so only the box's label, its underline width and the trailing notes are
/// kept.
struct FailureParts<'a> {
    message: &'a str,
    /// The `╰─ label` under the analyzer's underline.
    label: Option<&'a str>,
    /// How many characters the analyzer underlined.
    width: Option<usize>,
    /// `help: …`, `note: …` and any other line outside the box.
    notes: Vec<&'a str>,
}

fn failure_parts(message: &str) -> FailureParts<'_> {
    let mut lines = message.split('\n');
    let first = lines.next().unwrap_or("");
    let mut parts = FailureParts {
        message: first,
        label: None,
        width: None,
        notes: Vec::new(),
    };
    let mut in_box = false;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('╭') {
            in_box = true;
            continue;
        }
        if in_box {
            if trimmed.starts_with('╰') {
                in_box = false;
                continue;
            }
            // `  · ──┬─` is the underline, `  ·   ╰─ label` its label; the
            // numbered lines are the SQL as analyzed.
            if let Some(rest) = trimmed.strip_prefix('·') {
                if let Some(label) = rest.trim_start().strip_prefix("╰─") {
                    parts.label.get_or_insert(label.trim());
                } else if parts.width.is_none() {
                    let width = rest.chars().filter(|c| matches!(c, '─' | '┬')).count();
                    if width > 0 {
                        parts.width = Some(width);
                    }
                }
            }
            continue;
        }
        if !trimmed.is_empty() {
            parts.notes.push(trimmed);
        }
    }
    parts
}

/// How many characters the token at `column` (1-based) of `line` spans: an
/// identifier, a quoted identifier or string, or one character.
fn token_width(line: &str, column: u32) -> usize {
    let chars: Vec<char> = line
        .chars()
        .skip(column.saturating_sub(1) as usize)
        .collect();
    let Some(&first) = chars.first() else {
        return 1;
    };
    if first == '"' || first == '\'' {
        return chars[1..]
            .iter()
            .position(|&c| c == first)
            .map_or(1, |end| end + 2);
    }
    let word = chars
        .iter()
        .take_while(|c| c.is_alphanumeric() || **c == '_' || **c == '$')
        .count();
    word.max(1)
}

fn human_failure(failure: &Failure, sources: &BTreeMap<String, String>, colors: bool) -> String {
    let parts = failure_parts(&failure.message);
    let mut out = format!("{} {}", paint("1;31", "error:", colors), parts.message);
    let source = sources.get(&failure.path);
    match (failure.start, source) {
        (Some(start), Some(source)) => {
            let line_no = start.line.max(1);
            let text = source.split('\n').nth((line_no - 1) as usize).unwrap_or("");
            let gutter = line_no.to_string().len() + 1;
            let pad = " ".repeat(gutter + 1);
            out.push('\n');
            out.push_str(&paint(
                GUTTER,
                format!("{pad}╭─[{}:{}:{}]", failure.path, start.line, start.column),
                colors,
            ));
            out.push('\n');
            out.push_str(&format!(
                "{} {}",
                paint(GUTTER, format!("{line_no:>gutter$} │"), colors),
                text.replace('\t', "    ")
            ));
            let column = start.column.max(1);
            let from = display_width(text, column - 1);
            let width = parts.width.unwrap_or_else(|| token_width(text, column));
            let marker = if parts.label.is_some() {
                // `──┬─`: the connector under the middle of the token.
                let left = width / 2;
                format!(
                    "{}┬{}",
                    "─".repeat(left),
                    "─".repeat(width.saturating_sub(left + 1))
                )
            } else {
                "─".repeat(width)
            };
            out.push('\n');
            out.push_str(&paint(
                GUTTER,
                format!("{pad}· {}{marker}", " ".repeat(from)),
                colors,
            ));
            if let Some(label) = parts.label {
                out.push('\n');
                out.push_str(&paint(
                    GUTTER,
                    format!("{pad}· {}╰─ ", " ".repeat(from + width / 2)),
                    colors,
                ));
                out.push_str(&paint("1;31", label, colors));
            }
            out.push('\n');
            out.push_str(&paint(GUTTER, format!("{pad}╰────"), colors));
            for note in &parts.notes {
                out.push('\n');
                out.push_str(&format!(
                    "{}{}",
                    " ".repeat(gutter),
                    styled_note(note, colors)
                ));
            }
        }
        (start, _) => {
            let at = match start {
                Some(start) => format!("{}:{}:{}", failure.path, start.line, start.column),
                None => failure.path.clone(),
            };
            out.push('\n');
            out.push_str(&paint(GUTTER, format!("  --> {at}"), colors));
            for note in &parts.notes {
                out.push('\n');
                out.push_str(&format!("  {}", styled_note(note, colors)));
            }
        }
    }
    out
}

/// A `help: …` line with its label in green; anything else as written.
fn styled_note(note: &str, colors: bool) -> String {
    match note.strip_prefix("help:") {
        Some(rest) => format!("{}:{rest}", paint("32", "help", colors)),
        None => note.to_owned(),
    }
}

fn human_entry(entry: &Entry, sources: &BTreeMap<String, String>, colors: bool) -> String {
    let mut out = format!(
        "{}: {}",
        paint(
            severity_color(entry.finding.severity),
            entry.finding.label(),
            colors
        ),
        entry.finding.message
    );
    let source = sources.get(&entry.path).map(String::as_str).unwrap_or("");
    let lines: Vec<&str> = source.split('\n').collect();

    let start_line = entry.start.line.max(1);
    let mut end_line = entry.end.line.max(start_line);
    // An end at the start of a line belongs to the line before it.
    if entry.end.column <= 1 && end_line > start_line {
        end_line -= 1;
    }
    let span_lines = end_line - start_line + 1;
    let shown_end = if span_lines > 4 {
        start_line + 2
    } else {
        end_line
    };
    // The gutter fits the largest line number shown, with a leading space
    // before the number (miette's shape).
    let gutter = shown_end.to_string().len().max(1) + 1;
    let pad = " ".repeat(gutter + 1);
    out.push('\n');
    out.push_str(&paint(
        GUTTER,
        format!(
            "{pad}╭─[{}:{}:{}]",
            entry.path, entry.start.line, entry.start.column
        ),
        colors,
    ));
    for line_no in start_line..=shown_end {
        let text = lines.get((line_no - 1) as usize).copied().unwrap_or("");
        let expanded = text.replace('\t', "    ");
        out.push('\n');
        out.push_str(&format!(
            "{} {expanded}",
            paint(GUTTER, format!("{line_no:>gutter$} │"), colors),
        ));

        let from = if line_no == start_line {
            entry.start.column.max(1)
        } else {
            1
        };
        let chars = text.chars().count() as u32;
        let to = if line_no == end_line {
            entry.end.column.max(from + 1)
        } else {
            chars + 1
        };
        let from_display = display_width(text, from - 1);
        let to_display = display_width(text, to - 1).max(from_display + 1);
        let underline = format!(
            "{}{}",
            " ".repeat(from_display),
            "─".repeat(to_display - from_display)
        );
        out.push('\n');
        out.push_str(&paint(GUTTER, format!("{pad}· {underline}"), colors));
    }
    if span_lines > 4 {
        out.push('\n');
        out.push_str(&paint(GUTTER, format!("{pad}· …"), colors));
    }
    out.push('\n');
    out.push_str(&paint(GUTTER, format!("{pad}╰────"), colors));
    if let Some(help) = &entry.finding.help {
        out.push('\n');
        out.push_str(&format!(
            "{}{}: {help}",
            " ".repeat(gutter),
            paint("32", "help", colors),
        ));
    }
    out
}

fn format_duration(duration: Duration) -> String {
    let centis = duration.as_millis() / 10;
    format!("{}.{:02}s", centis / 100, centis % 100)
}

fn schema_counts(counts: &UserObjectCounts) -> String {
    let mut parts = Vec::new();
    let mut push = |n: usize, one: &str, many: &str| {
        if n > 0 {
            parts.push(if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {many}")
            });
        }
    };
    push(counts.tables, "table", "tables");
    push(counts.views, "view", "views");
    push(counts.indexes, "index", "indexes");
    push(counts.sequences, "sequence", "sequences");
    push(counts.types, "type", "types");
    push(counts.functions, "function", "functions");
    push(counts.extensions, "extension", "extensions");
    if parts.is_empty() {
        "empty".to_owned()
    } else {
        parts.join(", ")
    }
}

fn sarif_level(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "note",
    }
}

fn entries_region(entry: &Entry) -> serde_json::Value {
    serde_json::json!({
        "startLine": entry.start.line,
        "startColumn": entry.start.column,
        "endLine": entry.end.line,
        "endColumn": entry.end.column,
    })
}

fn location(path: &str, region: Option<serde_json::Value>) -> serde_json::Value {
    let mut physical = serde_json::json!({
        "artifactLocation": {
            "uri": path.replace('\\', "/"),
            "uriBaseId": "%SRCROOT%",
        },
    });
    if let Some(region) = region {
        physical["region"] = region;
    }
    serde_json::json!({ "physicalLocation": physical })
}

/// The report's output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// The boxes of `typedpg check`, with the summary on stderr.
    #[default]
    Human,
    /// One JSON object: findings, failures and the summary.
    Json,
    /// SARIF 2.1.0, for code-scanning dashboards.
    Sarif,
}

impl Format {
    /// The format's name on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Format::Human => "human",
            Format::Json => "json",
            Format::Sarif => "sarif",
        }
    }

    /// The format named `name` (see [`Format::name`]).
    pub fn from_name(name: &str) -> Option<Format> {
        [Format::Human, Format::Json, Format::Sarif]
            .into_iter()
            .find(|format| format.name() == name)
    }
}

impl FromStr for Format {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Format::from_name(name)
            .ok_or_else(|| format!("unknown format `{name}`: expected one of human, json, sarif"))
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Serialize)]
struct JsonReport<'a> {
    findings: Vec<JsonFinding<'a>>,
    failures: Vec<JsonFailure<'a>>,
    summary: JsonSummary<'a>,
}

#[derive(Serialize)]
struct JsonFinding<'a> {
    path: &'a str,
    line: u32,
    column: u32,
    #[serde(rename = "endLine")]
    end_line: u32,
    #[serde(rename = "endColumn")]
    end_column: u32,
    rule: &'static str,
    severity: &'static str,
    message: &'a str,
    help: Option<&'a str>,
}

#[derive(Serialize)]
struct JsonFailure<'a> {
    path: &'a str,
    line: Option<u32>,
    column: Option<u32>,
    message: &'a str,
}

#[derive(Serialize)]
struct JsonSummary<'a> {
    errors: usize,
    warnings: usize,
    infos: usize,
    queries: usize,
    migrations: usize,
    failures: usize,
    schema: BTreeMap<&'a str, &'a UserObjectCounts>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rule;

    fn finding(rule: Rule, severity: Severity, message: &str, help: Option<&str>) -> Finding {
        Finding {
            rule,
            severity,
            span: 0..1,
            message: message.to_owned(),
            help: help.map(str::to_owned),
        }
    }

    fn entry(path: &str, line: u32, column: u32, finding: Finding) -> Entry {
        Entry {
            path: path.to_owned(),
            start: Position { line, column },
            end: Position {
                line,
                column: column + 1,
            },
            finding,
        }
    }

    fn source(report: &mut Report, path: &str, text: &str) {
        report.sources.insert(path.to_owned(), text.to_owned());
    }

    #[test]
    fn position_counts_code_points() {
        let text = "aé\nbé";
        assert_eq!(position(text, 0), Position { line: 1, column: 1 });
        assert_eq!(position(text, 1), Position { line: 1, column: 2 });
        // `é` is two bytes but one column.
        assert_eq!(position(text, 3), Position { line: 1, column: 3 });
        assert_eq!(position(text, 4), Position { line: 2, column: 1 });
        assert_eq!(position(text, 6), Position { line: 2, column: 2 });
        assert_eq!(position(text, 7), Position { line: 2, column: 3 });
        assert_eq!(position(text, 100), Position { line: 2, column: 3 });
        // An offset inside a character rounds down to its start.
        assert_eq!(position(text, 2), Position { line: 1, column: 2 });
    }

    #[test]
    fn sorting_is_by_path_position_and_rule() {
        let mut report = Report {
            entries: vec![
                entry(
                    "b.rs",
                    1,
                    1,
                    finding(Rule::NullComparison, Severity::Error, "b", None),
                ),
                entry(
                    "a.rs",
                    2,
                    1,
                    finding(Rule::NullComparison, Severity::Error, "a2", None),
                ),
                entry(
                    "a.rs",
                    1,
                    2,
                    finding(Rule::UnboundedWrite, Severity::Error, "a1", None),
                ),
                entry(
                    "a.rs",
                    1,
                    1,
                    finding(Rule::NullComparison, Severity::Error, "a0", None),
                ),
            ],
            ..Report::default()
        };
        report.sort();
        let order: Vec<&str> = report
            .entries
            .iter()
            .map(|entry| entry.finding.message.as_str())
            .collect();
        assert_eq!(order, ["a0", "a1", "a2", "b"]);
    }

    #[test]
    fn json_is_one_document_with_the_pinned_keys() {
        let report = Report {
            entries: vec![entry(
                "src/a.rs",
                1,
                2,
                finding(
                    Rule::NullComparison,
                    Severity::Error,
                    "comparison with NULL is never true",
                    Some("use IS NULL / IS NOT NULL"),
                ),
            )],
            failures: vec![Failure {
                path: "src/b.rs".to_owned(),
                start: None,
                message: "boom".to_owned(),
            }],
            queries: 2,
            migrations: 3,
            schemas: vec![(
                "default".to_owned(),
                UserObjectCounts {
                    tables: 1,
                    views: 0,
                    indexes: 2,
                    sequences: 0,
                    types: 1,
                    functions: 0,
                    extensions: 0,
                },
            )],
            ..Report::default()
        };
        assert_eq!(
            report.render(Format::Json),
            r#"{
  "findings": [
    {
      "path": "src/a.rs",
      "line": 1,
      "column": 2,
      "endLine": 1,
      "endColumn": 3,
      "rule": "null_comparison",
      "severity": "error",
      "message": "comparison with NULL is never true",
      "help": "use IS NULL / IS NOT NULL"
    }
  ],
  "failures": [
    {
      "path": "src/b.rs",
      "line": null,
      "column": null,
      "message": "boom"
    }
  ],
  "summary": {
    "errors": 1,
    "warnings": 0,
    "infos": 0,
    "queries": 2,
    "migrations": 3,
    "failures": 1,
    "schema": {
      "default": {
        "tables": 1,
        "views": 0,
        "indexes": 2,
        "sequences": 0,
        "types": 1,
        "functions": 0,
        "extensions": 0
      }
    }
  }
}"#
        );
    }

    #[test]
    fn sarif_has_the_driver_rules_results_and_invocation() {
        let report = Report {
            entries: vec![
                entry(
                    "src/a.rs",
                    2,
                    3,
                    finding(Rule::RedundantIndex, Severity::Info, "redundant", None),
                ),
                entry(
                    "src/b.rs",
                    4,
                    5,
                    finding(
                        Rule::NullComparison,
                        Severity::Error,
                        "null comparison",
                        Some("use IS NULL"),
                    ),
                ),
            ],
            failures: vec![
                Failure {
                    path: "src/c.rs".to_owned(),
                    start: Some(Position { line: 7, column: 8 }),
                    message: "analysis failed".to_owned(),
                },
                Failure {
                    path: "migrations".to_owned(),
                    start: None,
                    message: "migration failed".to_owned(),
                },
            ],
            queries: 2,
            migrations: 1,
            ..Report::default()
        };
        let sarif: serde_json::Value = serde_json::from_str(&report.render(Format::Sarif)).unwrap();
        assert_eq!(
            sarif["$schema"],
            "https://json.schemastore.org/sarif-2.1.0.json"
        );
        assert_eq!(sarif["version"], "2.1.0");
        let run = &sarif["runs"][0];
        assert_eq!(run["columnKind"], "unicodeCodePoints");
        assert_eq!(run["invocations"][0]["executionSuccessful"], false);
        let driver = &run["tool"]["driver"];
        assert_eq!(driver["name"], "typedpg");
        assert_eq!(driver["informationUri"], "https://github.com/cubos/typedpg");
        assert_eq!(driver["version"], env!("CARGO_PKG_VERSION"));

        let rules = driver["rules"].as_array().unwrap();
        for rule in Rule::ALL.into_iter().chain([Rule::InvalidAllow]) {
            let rule_json = rules
                .iter()
                .find(|candidate| candidate["id"] == rule.name())
                .unwrap_or_else(|| panic!("rule {} is missing", rule.name()));
            assert_eq!(
                rule_json["shortDescription"]["text"],
                rule.description(),
                "{rule}"
            );
            assert_eq!(
                rule_json["defaultConfiguration"]["level"],
                sarif_level(rule.default_severity()),
                "{rule}"
            );
        }
        assert!(rules.iter().any(|rule| rule["id"] == ANALYSIS_ERROR));

        let results = run["results"].as_array().unwrap();
        assert_eq!(results.len(), 4);

        assert_eq!(results[0]["ruleId"], "redundant_index");
        assert_eq!(results[0]["level"], "note");
        assert_eq!(results[0]["message"]["text"], "redundant");
        let region = &results[0]["locations"][0]["physicalLocation"]["region"];
        assert_eq!(region["startLine"], 2);
        assert_eq!(region["startColumn"], 3);
        assert_eq!(region["endLine"], 2);
        assert_eq!(region["endColumn"], 4);
        let artifact = &results[0]["locations"][0]["physicalLocation"]["artifactLocation"];
        assert_eq!(artifact["uri"], "src/a.rs");
        assert_eq!(artifact["uriBaseId"], "%SRCROOT%");

        assert_eq!(results[1]["level"], "error");
        assert_eq!(
            results[1]["message"]["text"],
            "null comparison\nuse IS NULL"
        );

        assert_eq!(results[2]["ruleId"], ANALYSIS_ERROR);
        assert_eq!(results[2]["level"], "error");
        assert_eq!(results[2]["message"]["text"], "analysis failed");
        let region = &results[2]["locations"][0]["physicalLocation"]["region"];
        assert_eq!(region["startLine"], 7);
        assert_eq!(region["startColumn"], 8);

        assert_eq!(results[3]["ruleId"], ANALYSIS_ERROR);
        assert!(results[3]["locations"][0]["physicalLocation"]["region"].is_null());

        let passing = Report::default();
        let sarif: serde_json::Value =
            serde_json::from_str(&passing.render(Format::Sarif)).unwrap();
        assert_eq!(
            sarif["runs"][0]["invocations"][0]["executionSuccessful"],
            true
        );
    }

    #[test]
    fn failures_fail_the_run_and_human_puts_them_in_stdout() {
        let report = Report {
            entries: Vec::new(),
            failures: vec![Failure {
                path: "src/a.rs".to_owned(),
                start: Some(Position { line: 1, column: 2 }),
                message: "boom".to_owned(),
            }],
            ..Report::default()
        };
        assert!(report.fails(&LintOptions::default()));
        // Without the file's text there is no line to show: the position
        // alone.
        assert_eq!(
            report.render(Format::Human),
            "error: boom\n  --> src/a.rs:1:2"
        );
        let mut with_source = report.clone();
        source(&mut with_source, "src/a.rs", "let x = 1;\n");
        assert_eq!(
            with_source.render(Format::Human),
            "error: boom\n   ╭─[src/a.rs:1:2]\n 1 │ let x = 1;\n   ·  ──\n   ╰────"
        );
        assert_eq!(report.failures[0].render(), "src/a.rs:1:2: error: boom");
        let failure = Failure {
            path: "migrations".to_owned(),
            start: None,
            message: "boom".to_owned(),
        };
        assert_eq!(failure.render(), "migrations: error: boom");
    }

    #[test]
    fn human_draws_the_file_line_with_the_analyzer_label_and_help() {
        let mut report = Report {
            failures: vec![Failure {
                path: "src/bad.ts".to_owned(),
                start: Some(Position {
                    line: 12,
                    column: 25,
                }),
                message: "column \"nmae\" does not exist\n  ╭────\n1 │ SELECT id, nmae FROM users\n  ·            ──┬─\n  ·              ╰─ column does not exist\n  ╰────\n  help: Perhaps you meant \"users.name\".".to_owned(),
            }],
            ..Report::default()
        };
        let file = format!(
            "{}  sql(\"SELECT id, nmae FROM users\");\n",
            "\n".repeat(11)
        );
        source(&mut report, "src/bad.ts", &file);
        assert_eq!(
            report.render(Format::Human),
            "error: column \"nmae\" does not exist\n    ╭─[src/bad.ts:12:25]\n 12 │   sql(\"SELECT id, nmae FROM users\");\n    ·                         ──┬─\n    ·                           ╰─ column does not exist\n    ╰────\n   help: Perhaps you meant \"users.name\"."
        );
    }

    #[test]
    fn a_failure_without_a_position_keeps_its_notes() {
        let report = Report {
            failures: vec![Failure {
                path: "migrations".to_owned(),
                start: None,
                message: "migration failed\n  help: fix it".to_owned(),
            }],
            ..Report::default()
        };
        assert_eq!(
            report.render(Format::Human),
            "error: migration failed\n  --> migrations\n  help: fix it"
        );
    }

    #[test]
    fn a_quoted_token_is_underlined_whole() {
        assert_eq!(token_width("copy_in!(pool, \"missing (id)\")", 16), 14);
        assert_eq!(token_width("WHERE nope = 1", 7), 4);
        assert_eq!(token_width("a + b", 3), 1);
        assert_eq!(token_width("", 1), 1);
    }

    #[test]
    fn human_matches_the_pinned_box() {
        let text = "CREATE INDEX users_name_idx ON users (name);";
        let mut report = Report {
            entries: vec![Entry {
                path: "migrations/0002_hazards.sql".to_owned(),
                start: Position { line: 3, column: 1 },
                end: Position {
                    line: 3,
                    column: text.chars().count() as u32 + 1,
                },
                finding: finding(
                    Rule::CreateIndexBlocking,
                    Severity::Warning,
                    "CREATE INDEX without CONCURRENTLY blocks writes on public.users for the whole build",
                    Some(
                        "use CREATE INDEX CONCURRENTLY, in a migration whose first line is -- no-transaction",
                    ),
                ),
            }],
            ..Report::default()
        };
        source(
            &mut report,
            "migrations/0002_hazards.sql",
            &format!("\n\n{text}"),
        );
        assert_eq!(
            report.render(Format::Human),
            "warning[create_index_blocking]: CREATE INDEX without CONCURRENTLY blocks writes on public.users for the whole build\n   ╭─[migrations/0002_hazards.sql:3:1]\n 3 │ CREATE INDEX users_name_idx ON users (name);\n   · ────────────────────────────────────────────\n   ╰────\n  help: use CREATE INDEX CONCURRENTLY, in a migration whose first line is -- no-transaction"
        );
    }

    #[test]
    fn human_colors_the_severity_and_the_gutter() {
        let mut report = Report {
            entries: vec![Entry {
                path: "a.sql".to_owned(),
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
                finding: finding(Rule::NullComparison, Severity::Error, "bad", Some("fix it")),
            }],
            ..Report::default()
        };
        source(&mut report, "a.sql", "x = NULL");
        assert_eq!(
            report.render_with(Format::Human, &LintOptions::default(), true),
            "\x1b[1;31merror[null_comparison]\x1b[0m: bad\n\x1b[2;36m   ╭─[a.sql:1:1]\x1b[0m\n\x1b[2;36m 1 │\x1b[0m x = NULL\n\x1b[2;36m   · ─\x1b[0m\n\x1b[2;36m   ╰────\x1b[0m\n  \x1b[32mhelp\x1b[0m: fix it"
        );
    }

    #[test]
    fn gutter_widths_follow_the_largest_line_shown() {
        for (line, expected) in [(9, " 9 │ "), (10, " 10 │ "), (100, " 100 │ ")] {
            let path = format!("{line}.sql");
            let mut report = Report {
                entries: vec![Entry {
                    path: path.clone(),
                    start: Position { line, column: 1 },
                    end: Position { line, column: 2 },
                    finding: finding(Rule::NullComparison, Severity::Error, "bad", None),
                }],
                ..Report::default()
            };
            source(
                &mut report,
                &path,
                &format!("{}abcdef", "\n".repeat(line as usize - 1)),
            );
            let rendered = report.render(Format::Human);
            assert!(
                rendered.contains(&format!("\n{expected}abcdef")),
                "{rendered}"
            );
            // The header's gutter is one space wider than the line numbers'.
            let header = rendered
                .lines()
                .find(|line| line.contains("╭─["))
                .expect("header");
            assert!(
                header.starts_with(&" ".repeat(expected.chars().count() - 2)),
                "{header}"
            );
        }
    }

    #[test]
    fn multi_line_spans_underline_each_shown_line() {
        let mut report = Report {
            entries: vec![Entry {
                path: "m.sql".to_owned(),
                start: Position { line: 1, column: 3 },
                end: Position { line: 2, column: 4 },
                finding: finding(Rule::TableRewrite, Severity::Warning, "two lines", None),
            }],
            ..Report::default()
        };
        source(&mut report, "m.sql", "aaaaa\nbbbb\ncccc");
        assert_eq!(
            report.render(Format::Human),
            "warning[table_rewrite]: two lines\n   ╭─[m.sql:1:3]\n 1 │ aaaaa\n   ·   ───\n 2 │ bbbb\n   · ───\n   ╰────"
        );

        let mut report = Report {
            entries: vec![Entry {
                path: "m.sql".to_owned(),
                start: Position { line: 1, column: 1 },
                end: Position { line: 7, column: 2 },
                finding: finding(Rule::TableRewrite, Severity::Warning, "many lines", None),
            }],
            ..Report::default()
        };
        source(&mut report, "m.sql", "a\nb\nc\nd\ne\nf\ng");
        assert_eq!(
            report.render(Format::Human),
            "warning[table_rewrite]: many lines\n   ╭─[m.sql:1:1]\n 1 │ a\n   · ─\n 2 │ b\n   · ─\n 3 │ c\n   · ─\n   · …\n   ╰────"
        );
    }

    #[test]
    fn tabs_expand_to_four_spaces_in_the_line_and_the_underline() {
        let mut report = Report {
            entries: vec![Entry {
                path: "t.sql".to_owned(),
                start: Position { line: 1, column: 2 },
                end: Position { line: 1, column: 3 },
                finding: finding(Rule::NullComparison, Severity::Error, "tab", None),
            }],
            ..Report::default()
        };
        source(&mut report, "t.sql", "\tx");
        assert_eq!(
            report.render(Format::Human),
            "error[null_comparison]: tab\n   ╭─[t.sql:1:2]\n 1 │     x\n   ·     ─\n   ╰────"
        );
    }

    #[test]
    fn summary_block_pins_counts_schema_and_time() {
        let report = Report {
            entries: vec![entry(
                "a.sql",
                1,
                1,
                finding(
                    Rule::NullComparison,
                    Severity::Error,
                    "comparison with NULL is never true",
                    None,
                ),
            )],
            queries: 8,
            migrations: 2,
            schemas: vec![(
                "default".to_owned(),
                UserObjectCounts {
                    tables: 3,
                    views: 1,
                    indexes: 6,
                    sequences: 0,
                    types: 1,
                    functions: 2,
                    extensions: 0,
                },
            )],
            elapsed: Duration::from_millis(420),
            ..Report::default()
        };
        assert_eq!(
            report.summary(),
            "typedpg lint: 1 error\n  checked 8 queries and 2 migrations in 0.42s\n  schema: 3 tables, 1 view, 6 indexes, 1 type, 2 functions\n  ✖ failed: 1 finding at or above fail_on (error)"
        );
    }

    #[test]
    fn summary_several_databases_and_failures() {
        let report = Report {
            failures: vec![Failure {
                path: "src/a.rs".to_owned(),
                start: None,
                message: "boom".to_owned(),
            }],
            queries: 1,
            schemas: vec![
                ("default".to_owned(), UserObjectCounts::default()),
                (
                    "other".to_owned(),
                    UserObjectCounts {
                        tables: 1,
                        ..UserObjectCounts::default()
                    },
                ),
            ],
            elapsed: Duration::from_millis(5),
            ..Report::default()
        };
        assert_eq!(
            report.summary(),
            "typedpg lint: no findings\n  checked 1 query and 0 migrations in 0.00s\n  schema (default): empty\n  schema (other): 1 table\n  ✖ 1 query doesn't compile — the build would fail\n  ✖ failed: 1 analysis error"
        );
    }

    #[test]
    fn format_names_round_trip() {
        for format in [Format::Human, Format::Json, Format::Sarif] {
            assert_eq!(Format::from_name(format.name()), Some(format));
            assert_eq!(format.name().parse::<Format>().unwrap(), format);
        }
        assert!(Format::from_name("xml").is_none());
        assert!("xml".parse::<Format>().is_err());
    }

    #[test]
    fn a_run_without_findings_passes() {
        let report = Report {
            queries: 1,
            migrations: 1,
            schemas: vec![("default".to_owned(), UserObjectCounts::default())],
            elapsed: Duration::from_millis(1000),
            ..Report::default()
        };
        assert_eq!(
            report.summary(),
            "typedpg lint: no findings\n  checked 1 query and 1 migration in 1.00s\n  schema: empty\n  ✔ passed"
        );
    }
}
