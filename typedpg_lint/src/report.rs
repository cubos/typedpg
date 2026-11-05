//! The lint report both CLIs build and render.
//!
//! A report is one list of findings, one list of analysis failures, and the
//! counts [`Report::summary`] shows. [`Format::Human`] renders the findings
//! as the `path:line:column: severity[rule]: message` lines (with a
//! `  help: …` line when there is one) and leaves the failures for the
//! caller to print on stderr, as the CLIs always have; [`Format::Json`] and
//! [`Format::Sarif`] put the failures in the document too.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::Serialize;

use crate::{Finding, LintOptions, Rule, Severity, render, summary_line};

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
    /// The human-format line, for stderr:
    /// `path:line:column: error: message`, or `path: error: message` when
    /// the failure has no position.
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
    /// How many `sql!` sites were analyzed.
    pub queries: usize,
    /// How many migration files were applied.
    pub migrations: usize,
}

impl Report {
    /// Sort the findings by path, position and rule.
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| {
            (a.path.as_str(), a.start, a.finding.rule).cmp(&(
                b.path.as_str(),
                b.start,
                b.finding.rule,
            ))
        });
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

    /// The document printed to stdout in `format`. The human format lists
    /// only the findings: failures stay on stderr, as they always have.
    pub fn render(&self, format: Format) -> String {
        match format {
            Format::Human => self
                .entries
                .iter()
                .map(|entry| {
                    render(
                        Path::new(&entry.path),
                        entry.start.line,
                        entry.start.column,
                        &entry.finding,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Format::Json => self.json(),
            Format::Sarif => self.sarif(),
        }
    }

    /// The summary line, for stderr:
    /// `[typedpg] lint: 2 errors, 1 warning, 0 infos in 14 queries and 3 migrations`.
    pub fn summary(&self) -> String {
        let count = |severity| {
            self.entries
                .iter()
                .filter(|entry| entry.finding.severity == severity)
                .count()
        };
        summary_line(
            count(Severity::Error),
            count(Severity::Warning),
            count(Severity::Info),
            self.queries,
            self.migrations,
        )
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
        let document = JsonReport {
            findings,
            failures,
            summary: JsonSummary {
                errors: count(Severity::Error),
                warnings: count(Severity::Warning),
                infos: count(Severity::Info),
                queries: self.queries,
                migrations: self.migrations,
            },
        };
        serde_json::to_string_pretty(&document).expect("the report serializes")
    }

    fn sarif(&self) -> String {
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
                "columnKind": "unicodeCodePoints",
                "results": results,
            }],
        });
        serde_json::to_string_pretty(&document).expect("the report serializes")
    }
}

const ANALYSIS_ERROR: &str = "analysis_error";

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
    /// `path:line:column: severity[rule]: message`, the default.
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
    summary: JsonSummary,
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
struct JsonSummary {
    errors: usize,
    warnings: usize,
    infos: usize,
    queries: usize,
    migrations: usize,
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
            failures: Vec::new(),
            queries: 0,
            migrations: 0,
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
    "migrations": 3
  }
}"#
        );
    }

    #[test]
    fn sarif_has_the_driver_rules_and_results() {
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
        };
        let sarif: serde_json::Value = serde_json::from_str(&report.render(Format::Sarif)).unwrap();
        assert_eq!(
            sarif["$schema"],
            "https://json.schemastore.org/sarif-2.1.0.json"
        );
        assert_eq!(sarif["version"], "2.1.0");
        let run = &sarif["runs"][0];
        assert_eq!(run["columnKind"], "unicodeCodePoints");
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
    }

    #[test]
    fn failures_fail_the_run_and_human_keeps_them_out_of_stdout() {
        let report = Report {
            entries: Vec::new(),
            failures: vec![Failure {
                path: "src/a.rs".to_owned(),
                start: Some(Position { line: 1, column: 2 }),
                message: "boom".to_owned(),
            }],
            queries: 0,
            migrations: 0,
        };
        assert!(report.fails(&LintOptions::default()));
        assert_eq!(report.render(Format::Human), "");
        assert_eq!(report.failures[0].render(), "src/a.rs:1:2: error: boom");
        let failure = Failure {
            path: "migrations".to_owned(),
            start: None,
            message: "boom".to_owned(),
        };
        assert_eq!(failure.render(), "migrations: error: boom");
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
}
