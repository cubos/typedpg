//! The binding rules: `redundant_binding` and `renaming_binding`.
//!
//! Both run over the Rust source of a `sql!` invocation — the bindings after
//! the SQL literal — with the template's `$name` / `$..name` occurrences for
//! context. A finding's span and edits are file byte offsets, so the CLI can
//! report it like any other finding and apply its edits with `--fix`.

use std::collections::HashSet;
use std::ops::Range;

use typedpg_analyzer::{TemplateParam, template_params};
use typedpg_lint::{Finding, LintOptions, Rule, allowed_rules};

use super::scan;

/// A binding finding and the source edits that fix it.
pub(crate) struct BindingFinding {
    /// The finding, with its span in **file** byte offsets.
    pub finding: Finding,
    /// File byte ranges and their replacements. A binding is always
    /// removed; a rename also rewrites its parameter's occurrences.
    pub edits: Vec<(Range<usize>, String)>,
}

/// The binding findings of one `sql!` invocation.
///
/// `sql` is the literal's cooked value (the template), `literal` maps it
/// back into the file, and `bindings` are the macro's bindings as written.
/// The configured level and the template's allow comments decide whether a
/// finding is reported, like any other rule.
pub(crate) fn binding_findings(
    sql: &str,
    literal: &scan::Literal,
    bindings: &[scan::Binding],
    options: &LintOptions,
) -> Vec<BindingFinding> {
    // A template that does not lex has no parameters to judge the bindings
    // against; its lex error is reported by the query analysis.
    let Some(params) = template_params(sql) else {
        return Vec::new();
    };
    let allowed = allowed_rules(sql);
    // The macro rejects a name bound twice: there is no one binding to
    // blame, so neither is reported.
    let mut seen = HashSet::new();
    let mut duplicated = HashSet::new();
    for binding in bindings {
        if !seen.insert(binding.name.as_str()) {
            duplicated.insert(binding.name.as_str());
        }
    }
    let binding_names: HashSet<&str> = bindings
        .iter()
        .map(|binding| binding.name.as_str())
        .collect();
    let mut findings = Vec::new();
    for binding in bindings {
        if duplicated.contains(binding.name.as_str()) {
            continue;
        }
        let value = match &binding.value {
            scan::BindingValue::Capture => Some(binding.name.as_str()),
            scan::BindingValue::Variable(variable) => Some(variable.as_str()),
            scan::BindingValue::Other => None,
        };
        match value {
            // `x = x`, or a bare `x`: the parameter captures `x` anyway.
            Some(variable) if variable == binding.name => {
                let written = match &binding.value {
                    scan::BindingValue::Capture => binding.name.clone(),
                    _ => format!("{} = {variable}", binding.name),
                };
                let sigil = sigil(&params, &binding.name);
                let message = format!(
                    "`{written}` binds `{sigil}{}` to the variable it captures without a binding",
                    binding.name
                );
                if let Some(finding) = report(
                    Rule::RedundantBinding,
                    binding,
                    &allowed,
                    options,
                    message,
                    "remove the binding: a parameter without one captures the variable of its \
                     name (`cargo typedpg lint --fix` removes it)",
                    vec![(binding.removal.clone(), String::new())],
                ) {
                    findings.push(finding);
                }
            }
            // `p = v`, `v != p`: the parameter could just be `$v`.
            Some(variable) => {
                if let Some(finding) = renaming(
                    binding,
                    variable,
                    &params,
                    bindings,
                    &binding_names,
                    literal,
                    options,
                    &allowed,
                ) {
                    findings.push(finding);
                }
            }
            None => {}
        }
    }
    findings
}

/// A `renaming_binding` finding for `binding = variable`, when renaming is
/// meaningful and unambiguous.
#[allow(clippy::too_many_arguments)]
fn renaming(
    binding: &scan::Binding,
    variable: &str,
    params: &[TemplateParam],
    bindings: &[scan::Binding],
    binding_names: &HashSet<&str>,
    literal: &scan::Literal,
    options: &LintOptions,
    allowed: &HashSet<Rule>,
) -> Option<BindingFinding> {
    // SCREAMING_CASE names a const or static by convention; `$MAX_ROWS`
    // reads as a constant, not a parameter.
    if !variable.chars().any(|c| c.is_lowercase()) {
        return None;
    }
    // The parameter name is not in the SQL: the macro rejects the binding
    // itself, so there is nothing to rename.
    if !params.iter().any(|param| param.name == binding.name) {
        return None;
    }
    // Renaming would merge two parameters, or collide with another binding.
    if params.iter().any(|param| param.name == variable) {
        return None;
    }
    if binding_names.contains(variable) {
        return None;
    }
    let binds_variable = |other: &scan::Binding| match &other.value {
        scan::BindingValue::Capture => other.name == variable,
        scan::BindingValue::Variable(value) => value == variable,
        scan::BindingValue::Other => false,
    };
    if bindings
        .iter()
        .filter(|other| binds_variable(other))
        .count()
        > 1
    {
        return None;
    }

    let sigil = sigil(params, &binding.name);
    let mut edits: Vec<(Range<usize>, String)> = params
        .iter()
        .filter(|param| param.name == binding.name)
        .map(|param| {
            (
                literal.source_offset(param.name_span.start)
                    ..literal.source_offset(param.name_span.end),
                variable.to_owned(),
            )
        })
        .collect();
    edits.push((binding.removal.clone(), String::new()));
    report(
        Rule::RenamingBinding,
        binding,
        allowed,
        options,
        format!(
            "`{} = {variable}` only renames the variable `{variable}`",
            binding.name
        ),
        format!(
            "write `{sigil}{variable}` in the SQL and remove the binding (`cargo typedpg lint \
             --fix` does it)"
        ),
        edits,
    )
}

/// The [`BindingFinding`] for `binding` at `rule`'s configured level, unless
/// the rule is off or the template's allow comments suppress it.
fn report(
    rule: Rule,
    binding: &scan::Binding,
    allowed: &HashSet<Rule>,
    options: &LintOptions,
    message: String,
    help: impl Into<String>,
    edits: Vec<(Range<usize>, String)>,
) -> Option<BindingFinding> {
    let severity = options.level(rule)?;
    if allowed.contains(&rule) {
        return None;
    }
    Some(BindingFinding {
        finding: Finding {
            rule,
            severity,
            span: binding.span.clone(),
            message,
            help: Some(help.into()),
        },
        edits,
    })
}

/// The way `params` writes parameter `name`: `$name`, or `$..name` when it
/// occurs only as a spread.
fn sigil(params: &[TemplateParam], name: &str) -> &'static str {
    let mut plain = false;
    let mut spread = false;
    for param in params.iter().filter(|param| param.name == name) {
        if param.spread {
            spread = true;
        } else {
            plain = true;
        }
    }
    if spread && !plain { "$.." } else { "$" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use typedpg_core::config::{LintConfig, LintLevel};
    use typedpg_lint::{LintOptions, Severity};

    /// The findings of the first `sql!` in `source`, at the default levels.
    fn findings(source: &str) -> Vec<BindingFinding> {
        findings_with(source, &LintOptions::default())
    }

    fn findings_with(source: &str, options: &LintOptions) -> Vec<BindingFinding> {
        let scanned = scan::scan(source).unwrap_or_else(|error| panic!("{error}: {source}"));
        let query = &scanned.queries[0];
        binding_findings(&query.text, &query.literal, &query.bindings, options)
    }

    /// Apply every finding's edits to `source`.
    fn fixed(source: &str, findings: &[BindingFinding]) -> String {
        let mut edits: Vec<(Range<usize>, String)> = findings
            .iter()
            .flat_map(|finding| finding.edits.iter().cloned())
            .collect();
        edits.sort_by_key(|(range, _)| range.start);
        let mut text = source.to_owned();
        for (range, replacement) in edits.into_iter().rev() {
            text.replace_range(range, &replacement);
        }
        text
    }

    fn level(rule: &str, level: LintLevel) -> LintOptions {
        let mut config = LintConfig::default();
        config.levels.insert(rule.to_owned(), level);
        LintOptions::from_config(&config)
    }

    #[test]
    fn a_self_binding_is_redundant() {
        let source = r#"fn main() { sql!(pool, "SELECT $principal", principal = principal); }"#;
        let findings = findings(source);
        assert_eq!(findings.len(), 1);
        let finding = &findings[0].finding;
        assert_eq!(finding.rule, Rule::RedundantBinding);
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(
            finding.message,
            "`principal = principal` binds `$principal` to the variable it captures without a \
             binding"
        );
        assert_eq!(
            finding.help.as_deref(),
            Some(
                "remove the binding: a parameter without one captures the variable of its name \
                 (`cargo typedpg lint --fix` removes it)"
            )
        );
        assert_eq!(
            fixed(source, &findings),
            r#"fn main() { sql!(pool, "SELECT $principal"); }"#
        );
    }

    #[test]
    fn a_bare_binding_is_redundant() {
        let source = r#"fn main() { sql!(pool, "SELECT $principal", principal); }"#;
        let findings = findings(source);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].finding.message,
            "`principal` binds `$principal` to the variable it captures without a binding"
        );
        assert_eq!(
            fixed(source, &findings),
            r#"fn main() { sql!(pool, "SELECT $principal"); }"#
        );
    }

    #[test]
    fn a_spread_binding_reports_the_spread_sigil() {
        let source = r#"fn main() { sql!(pool, "INSERT INTO t VALUES $..items", items = items); }"#;
        let findings = findings(source);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].finding.message,
            "`items = items` binds `$..items` to the variable it captures without a binding"
        );
    }

    #[test]
    fn a_rename_rewrites_every_occurrence() {
        let source =
            r#"fn main() { sql!(pool, "SELECT $document?, $document", document = next); }"#;
        let findings = findings(source);
        assert_eq!(findings.len(), 1);
        let finding = &findings[0].finding;
        assert_eq!(finding.rule, Rule::RenamingBinding);
        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(
            finding.message,
            "`document = next` only renames the variable `next`"
        );
        assert_eq!(
            finding.help.as_deref(),
            Some(
                "write `$next` in the SQL and remove the binding (`cargo typedpg lint --fix` \
                 does it)"
            )
        );
        assert_eq!(&source[finding.span.clone()], "document = next");
        assert_eq!(
            fixed(source, &findings),
            r#"fn main() { sql!(pool, "SELECT $next?, $next"); }"#
        );
    }

    #[test]
    fn a_spread_rename_uses_the_spread_sigil() {
        let source = r#"fn main() { sql!(pool, "INSERT INTO t VALUES $..items", items = rows); }"#;
        let findings = findings(source);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].finding.help.as_deref(),
            Some(
                "write `$..rows` in the SQL and remove the binding (`cargo typedpg lint --fix` \
                 does it)"
            )
        );
        assert_eq!(
            fixed(source, &findings),
            r#"fn main() { sql!(pool, "INSERT INTO t VALUES $..rows"); }"#
        );
    }

    #[test]
    fn a_raw_string_literal_is_fixed_too() {
        let source =
            r####"fn main() { sql!(pool, r#"SELECT $document FROM t"#, document = next); }"####;
        let findings = findings(source);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            fixed(source, &findings),
            r####"fn main() { sql!(pool, r#"SELECT $next FROM t"#); }"####
        );
    }

    #[test]
    fn only_plain_variables_are_bindings() {
        for value in ["&b", "b.c", "(b)", "b.clone()", "1", "b::c", "self"] {
            let source = format!(r#"fn main() {{ sql!(pool, "SELECT $a", a = {value}); }}"#);
            assert!(findings(&source).is_empty(), "{value}: {source}");
        }
    }

    #[test]
    fn a_renaming_binding_does_not_report_when_renaming_is_unsafe() {
        for (sql, bindings) in [
            // A SCREAMING_CASE value is a const by convention.
            ("SELECT $limit", "limit = MAX_ROWS"),
            // `$rows` is already a parameter: the rename would merge them.
            ("SELECT $items, $rows", "items = rows"),
            ("SELECT $items, $..rows", "items = rows"),
            // `rows` is another binding's name.
            ("SELECT $items, $rows", "items = rows, rows = 1"),
            // Two bindings would become the same parameter.
            ("SELECT $a, $b", "a = x, b = x"),
        ] {
            let source = format!(r#"fn main() {{ sql!(pool, "{sql}", {bindings}); }}"#);
            assert!(findings(&source).is_empty(), "{sql}: {bindings}");
        }
    }

    #[test]
    fn a_binding_whose_parameter_is_not_in_the_sql_does_not_rename() {
        let source = r#"fn main() { sql!(pool, "SELECT $other", p = next); }"#;
        assert!(findings(source).is_empty());
    }

    #[test]
    fn a_name_bound_twice_is_skipped() {
        let source = r#"fn main() { sql!(pool, "SELECT $x", x = x, x = x); }"#;
        assert!(findings(source).is_empty());
    }

    #[test]
    fn a_template_that_does_not_lex_has_no_binding_findings() {
        let source = r#"fn main() { sql!(pool, "SELECT $x 'unclosed", x = y); }"#;
        assert!(findings(source).is_empty());
    }

    #[test]
    fn allow_comments_and_levels_apply() {
        let redundant = r#"fn main() { sql!(pool, "SELECT $x -- typedpg-lint: allow(redundant_binding)", x = x); }"#;
        assert!(findings(redundant).is_empty());

        let renaming = r#"fn main() { sql!(pool, "SELECT $x -- typedpg-lint: allow(renaming_binding)", x = y); }"#;
        assert!(findings(renaming).is_empty());

        let source = r#"fn main() { sql!(pool, "SELECT $x", x = y); }"#;
        assert!(findings_with(source, &level("renaming_binding", LintLevel::Off)).is_empty());
        let findings = findings_with(source, &level("renaming_binding", LintLevel::Error));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].finding.severity, Severity::Error);

        let source = r#"fn main() { sql!(pool, "SELECT $x", x = x); }"#;
        assert!(findings_with(source, &level("redundant_binding", LintLevel::Off)).is_empty());
        let findings = findings_with(source, &level("redundant_binding", LintLevel::Info));
        assert_eq!(findings[0].finding.severity, Severity::Info);
    }

    #[test]
    fn both_fixes_rewrite_the_motivating_example() {
        let source = r#"fn main() {
    sql!(
        tx,
        "INSERT INTO s (principal, document) \
         VALUES ($principal, $document) \
         ON CONFLICT (principal) DO UPDATE \
            SET document = EXCLUDED.document, updated_at = now()",
        principal = principal,
        document = next,
    );
}
"#;
        let findings = findings(source);
        assert_eq!(findings.len(), 2, "{:?}", findings.len());
        let rules: Vec<Rule> = findings
            .iter()
            .map(|finding| finding.finding.rule)
            .collect();
        assert_eq!(rules, [Rule::RedundantBinding, Rule::RenamingBinding]);
        assert_eq!(
            fixed(source, &findings),
            r#"fn main() {
    sql!(
        tx,
        "INSERT INTO s (principal, document) \
         VALUES ($principal, $next) \
         ON CONFLICT (principal) DO UPDATE \
            SET document = EXCLUDED.document, updated_at = now()",
    );
}
"#
        );
    }
}
