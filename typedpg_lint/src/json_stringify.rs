//! `json_stringify_param`: a `JSON.stringify(…)` call passed as a `json` /
//! `jsonb` value.
//!
//! The TypeScript runtime JSON-encodes a `json` / `jsonb` value itself, and
//! a string is a valid JSON value: `{ doc: JSON.stringify(x) }` type-checks
//! and stores the JSON *string* `"{\"a\":1}"` instead of `x`. The caller
//! sees, syntactically, where a call passes a `JSON.stringify(…)` call
//! ([`Call::stringified`]); this module reports those whose parameter,
//! spread field or COPY column is `json` / `jsonb` (or a domain over one).
//! There is no fix: removing the call changes what is stored.

use typedpg_analyzer::{AnalyzeError, AnalyzedQuery, PgCatalog, SpreadKind, Type};

use crate::call_site::RawCallFinding;
use crate::{Call, CallFinding, Finding, LintOptions, RawFinding, Rule, StringifiedValue};

/// What the help line says.
const HELP: &str = "pass the value itself: typedpg encodes it as JSON (`jsonNull` for a JSON null)";

/// The findings of `json_stringify_param` for a query called as `calls`.
pub(crate) fn query_findings(analyzed: &AnalyzedQuery, calls: &[Call]) -> Vec<RawCallFinding> {
    let mut out = Vec::new();
    for (call_index, call) in calls.iter().enumerate() {
        for (value_index, value) in call.stringified.iter().enumerate() {
            let Some(subject) = query_subject(analyzed, value) else {
                continue;
            };
            out.push(RawCallFinding {
                call: Some(call_index),
                value: Some(value_index),
                finding: finding(&subject),
            });
        }
    }
    out
}

/// What a message calls the `json` / `jsonb` value `value` is, in a query:
/// `parameter $doc is jsonb`. `None` when it is not `json` / `jsonb`.
fn query_subject(analyzed: &AnalyzedQuery, value: &StringifiedValue) -> Option<String> {
    match value {
        StringifiedValue::Param(name) => {
            let param = analyzed.params.iter().find(|p| p.name == *name)?;
            Some(format!(
                "parameter ${name} is {}",
                json_kind(&param.pg_type)?
            ))
        }
        StringifiedValue::Element(name) => {
            if let Some(param) = analyzed.params.iter().find(|p| p.name == *name) {
                let element = array_element(&param.pg_type)?;
                return Some(format!(
                    "an element of parameter ${name} is {}",
                    json_kind(element)?
                ));
            }
            let spread = analyzed
                .spreads
                .iter()
                .find(|s| s.name == *name && s.kind == SpreadKind::List)?;
            let element = spread.fields.first()?;
            Some(format!(
                "an element of $..{name} is {}",
                json_kind(&element.pg_type)?
            ))
        }
        StringifiedValue::Field { spread, field } => {
            let spread_info = analyzed
                .spreads
                .iter()
                .find(|s| s.name == *spread && s.kind == SpreadKind::Rows)?;
            let field_info = spread_info.fields.iter().find(|f| f.name == *field)?;
            Some(format!(
                "field {field} of $..{spread} is {}",
                json_kind(&field_info.pg_type)?
            ))
        }
        StringifiedValue::Column(_) => None,
    }
}

fn finding(subject: &str) -> RawFinding {
    RawFinding {
        span: 0..0,
        message: format!("{subject}, and `JSON.stringify(…)` stores a JSON string, not the value"),
        help: Some(HELP.to_owned()),
        fix: Vec::new(),
    }
}

/// `json` or `jsonb` when `ty` is one, `public.prefs, a domain over jsonb`
/// when it is a domain over one; `None` otherwise.
fn json_kind(ty: &Type) -> Option<String> {
    let mut inner = ty;
    while let Type::Domain { base, .. } = inner {
        inner = base;
    }
    let Type::Basic { schema, name, .. } = inner else {
        return None;
    };
    if schema != "pg_catalog" || (name != "json" && name != "jsonb") {
        return None;
    }
    Some(if matches!(ty, Type::Domain { .. }) {
        format!("{}, a domain over {name}", ty.describe())
    } else {
        name.clone()
    })
}

/// The element type of an array (or of a domain over one).
fn array_element(ty: &Type) -> Option<&Type> {
    let mut inner = ty;
    while let Type::Domain { base, .. } = inner {
        inner = base;
    }
    match inner {
        Type::Array { element, .. } => Some(element),
        _ => None,
    }
}

/// `json_stringify_param` for the `copyIn()` target `target` called as
/// `calls`: a `JSON.stringify(…)` passed as a `json` / `jsonb` column's
/// value in the rows ([`StringifiedValue::Column`]).
///
/// The target is resolved as `typedpg check` resolves it: along the first
/// of `roles`' search paths, or the schema owner's with no roles. Every
/// finding is placed at a call's value (`call` and `value` are set); the
/// target takes no allow comment, so only the rule's level silences it.
/// Returns the target's analysis error when it does not analyze.
pub fn lint_copy_in_called(
    catalog: &PgCatalog,
    target: &str,
    calls: &[Call],
    roles: &[String],
    options: &LintOptions,
) -> Result<Vec<CallFinding>, AnalyzeError> {
    let Some(severity) = options.level(Rule::JsonStringifyParam) else {
        return Ok(Vec::new());
    };
    if calls.iter().all(|call| call.stringified.is_empty()) {
        return Ok(Vec::new());
    }
    let analyzed = match roles.first() {
        None => catalog.analyze_copy_in(target)?,
        Some(role) => catalog.analyze_copy_in_as(target, role)?,
    };
    let mut out = Vec::new();
    for (call_index, call) in calls.iter().enumerate() {
        for (value_index, value) in call.stringified.iter().enumerate() {
            let StringifiedValue::Column(name) = value else {
                continue;
            };
            let Some(column) = analyzed.columns.iter().find(|c| c.name == *name) else {
                continue;
            };
            let Some(kind) = json_kind(&column.pg_type) else {
                continue;
            };
            let raw = finding(&format!("column {name} is {kind}"));
            out.push(CallFinding {
                call: Some(call_index),
                value: Some(value_index),
                finding: Finding {
                    rule: Rule::JsonStringifyParam,
                    severity,
                    span: 0..0,
                    message: raw.message,
                    help: raw.help,
                    fix: Vec::new(),
                },
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::{
        Call, CallFinding, CallMethod, Language, LintOptions, Rule, Severity, StringifiedValue,
        lint_copy_in_called, lint_query_called,
    };

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE DOMAIN prefs AS jsonb;
                 CREATE DOMAIN deep AS prefs;
                 CREATE DOMAIN doc_list AS jsonb[];
                 CREATE TABLE docs (
                     id integer PRIMARY KEY,
                     body jsonb NOT NULL,
                     raw json,
                     settings prefs,
                     nested deep,
                     tags jsonb[],
                     list doc_list,
                     title text
                 );",
            )
            .unwrap();
        catalog
    }

    fn call(method: CallMethod, name: &str, stringified: Vec<StringifiedValue>) -> Call {
        Call {
            method,
            name: name.to_owned(),
            language: Language::TypeScript,
            stringified,
        }
    }

    fn param(name: &str) -> StringifiedValue {
        StringifiedValue::Param(name.to_owned())
    }

    fn element(name: &str) -> StringifiedValue {
        StringifiedValue::Element(name.to_owned())
    }

    fn findings(sql: &str, calls: &[Call]) -> Vec<CallFinding> {
        lint_query_called(&catalog(), sql, calls, &LintOptions::default())
            .unwrap()
            .into_iter()
            .filter(|f| f.finding.rule == Rule::JsonStringifyParam)
            .collect()
    }

    fn messages(sql: &str, calls: &[Call]) -> Vec<String> {
        findings(sql, calls)
            .into_iter()
            .map(|f| f.finding.message)
            .collect()
    }

    #[test]
    fn a_stringified_jsonb_parameter_is_an_error_placed_at_the_value() {
        let calls = [call(
            CallMethod::Execute,
            "execute",
            vec![param("id"), param("body")],
        )];
        let found = findings("INSERT INTO docs (id, body) VALUES ($id, $body)", &calls);
        assert_eq!(found.len(), 1, "{found:?}");
        let placed = &found[0];
        assert_eq!(placed.call, Some(0));
        // The second value the call passes: `body`, not `id`.
        assert_eq!(placed.value, Some(1));
        assert_eq!(placed.finding.severity, Severity::Error);
        assert_eq!(
            placed.finding.message,
            "parameter $body is jsonb, and `JSON.stringify(…)` stores a JSON string, not the value"
        );
        assert_eq!(
            placed.finding.help.as_deref(),
            Some("pass the value itself: typedpg encodes it as JSON (`jsonNull` for a JSON null)")
        );
        assert!(placed.finding.fix.is_empty());
    }

    #[test]
    fn json_and_domains_over_jsonb_are_reported() {
        let calls = [call(
            CallMethod::Execute,
            "execute",
            vec![param("raw"), param("settings"), param("nested")],
        )];
        assert_eq!(
            messages(
                "UPDATE docs SET raw = $raw, settings = $settings, nested = $nested WHERE id = 1",
                &calls
            ),
            [
                "parameter $raw is json, and `JSON.stringify(…)` stores a JSON string, not the \
                 value",
                "parameter $settings is public.prefs, a domain over jsonb, and \
                 `JSON.stringify(…)` stores a JSON string, not the value",
                "parameter $nested is public.deep, a domain over jsonb, and \
                 `JSON.stringify(…)` stores a JSON string, not the value",
            ]
        );
    }

    #[test]
    fn a_text_parameter_is_not_reported() {
        // Storing JSON text in a text column is what the code asks for.
        let calls = [call(CallMethod::Execute, "execute", vec![param("title")])];
        assert!(messages("UPDATE docs SET title = $title WHERE id = 1", &calls).is_empty());
        // A jsonb parameter whose value is the text cast in SQL: the
        // parameter itself is text, and `::jsonb` parses it.
        let calls = [call(CallMethod::Execute, "execute", vec![param("body")])];
        assert!(
            messages(
                "UPDATE docs SET body = $body::text::jsonb WHERE id = 1",
                &calls
            )
            .is_empty()
        );
        // A name the query has no parameter for is tsc's to reject.
        let calls = [call(CallMethod::Execute, "execute", vec![param("nope")])];
        assert!(messages("UPDATE docs SET body = $body WHERE id = 1", &calls).is_empty());
    }

    #[test]
    fn an_element_of_a_jsonb_array_parameter_is_reported() {
        let calls = [call(
            CallMethod::Execute,
            "execute",
            vec![element("tags"), element("list"), param("tags")],
        )];
        assert_eq!(
            messages(
                "UPDATE docs SET tags = $tags, list = $list WHERE id = 1",
                &calls
            ),
            [
                "an element of parameter $tags is jsonb, and `JSON.stringify(…)` stores a JSON \
                 string, not the value",
                "an element of parameter $list is jsonb, and `JSON.stringify(…)` stores a JSON \
                 string, not the value",
            ]
        );
        // An element of a scalar parameter is not one of an array.
        let calls = [call(CallMethod::Execute, "execute", vec![element("body")])];
        assert!(messages("UPDATE docs SET body = $body WHERE id = 1", &calls).is_empty());
    }

    #[test]
    fn spreads_fields_and_list_elements_are_reported() {
        let calls = [call(
            CallMethod::Execute,
            "execute",
            vec![
                StringifiedValue::Field {
                    spread: "rows".to_owned(),
                    field: "id".to_owned(),
                },
                StringifiedValue::Field {
                    spread: "rows".to_owned(),
                    field: "body".to_owned(),
                },
                // Not a spread: the scanner saw a field of an object in an
                // array literal, but `$rows` is the spread's whole value.
                StringifiedValue::Field {
                    spread: "other".to_owned(),
                    field: "body".to_owned(),
                },
            ],
        )];
        assert_eq!(
            messages(
                "INSERT INTO docs (id, body) VALUES $..rows { id, body }",
                &calls
            ),
            [
                "field body of $..rows is jsonb, and `JSON.stringify(…)` stores a JSON string, not \
              the value"
            ]
        );
        let calls = [call(
            CallMethod::FetchAll,
            "fetchAll",
            vec![element("bodies")],
        )];
        assert_eq!(
            messages("SELECT id FROM docs WHERE body IN $..bodies", &calls),
            [
                "an element of $..bodies is jsonb, and `JSON.stringify(…)` stores a JSON string, \
              not the value"
            ]
        );
    }

    #[test]
    fn every_call_and_its_values_are_judged_on_their_own() {
        let calls = [
            call(CallMethod::Execute, "execute", vec![]),
            call(CallMethod::Execute, "execute", vec![param("body")]),
            call(
                CallMethod::FetchAll,
                "fetchAll",
                vec![param("body"), param("body")],
            ),
        ];
        let found = findings("UPDATE docs SET body = $body WHERE id = 1", &calls);
        let placed: Vec<(Option<usize>, Option<usize>)> =
            found.iter().map(|f| (f.call, f.value)).collect();
        assert_eq!(
            placed,
            [(Some(1), Some(0)), (Some(2), Some(0)), (Some(2), Some(1))]
        );
    }

    #[test]
    fn the_rule_is_configurable_and_suppressible() {
        let calls = [call(CallMethod::Execute, "execute", vec![param("body")])];
        let sql = "UPDATE docs SET body = $body WHERE id = 1";
        let off = LintOptions::default().with_rule_off(Rule::JsonStringifyParam);
        assert!(
            lint_query_called(&catalog(), sql, &calls, &off)
                .unwrap()
                .iter()
                .all(|f| f.finding.rule != Rule::JsonStringifyParam)
        );
        let allowed = format!("-- typedpg-lint: allow(json_stringify_param)\n{sql}");
        assert!(messages(&allowed, &calls).is_empty());
    }

    #[test]
    fn copy_in_columns_are_reported() {
        let calls = [call(
            CallMethod::Execute,
            "execute",
            vec![
                StringifiedValue::Column("id".to_owned()),
                StringifiedValue::Column("body".to_owned()),
                StringifiedValue::Column("settings".to_owned()),
                StringifiedValue::Column("title".to_owned()),
                // Not a column of the target.
                StringifiedValue::Column("raw".to_owned()),
                // Only query parameters have these.
                param("body"),
            ],
        )];
        let found = lint_copy_in_called(
            &catalog(),
            "docs (id, body, settings, title)",
            &calls,
            &[],
            &LintOptions::default(),
        )
        .unwrap();
        let placed: Vec<(Option<usize>, Option<usize>, &str)> = found
            .iter()
            .map(|f| (f.call, f.value, f.finding.message.as_str()))
            .collect();
        assert_eq!(
            placed,
            [
                (
                    Some(0),
                    Some(1),
                    "column body is jsonb, and `JSON.stringify(…)` stores a JSON string, not the \
                     value"
                ),
                (
                    Some(0),
                    Some(2),
                    "column settings is public.prefs, a domain over jsonb, and \
                     `JSON.stringify(…)` stores a JSON string, not the value"
                ),
            ]
        );
        assert!(found.iter().all(|f| f.finding.severity == Severity::Error));
        let off = LintOptions::default().with_rule_off(Rule::JsonStringifyParam);
        assert!(
            lint_copy_in_called(&catalog(), "docs (id, body)", &calls, &[], &off)
                .unwrap()
                .is_empty()
        );
    }
}
