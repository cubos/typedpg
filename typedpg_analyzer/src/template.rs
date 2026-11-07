//! The named parameters and spreads of a `sql!` template, as written.
//!
//! The lexer deduplicates a parameter and records only its rewritten
//! offsets; the lint's binding rules need one entry per occurrence and the
//! byte range of its name, so they can rename every `$name` of the SQL
//! literal. [`template_params`] exposes exactly that.

use std::ops::Range;

/// A `$name` parameter or `$..name` spread occurrence of a `sql!` template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateParam {
    /// The name without the `$` / `$..` sigil and without any `?`/`!`
    /// annotation.
    pub name: String,
    /// Whether this occurrence is a `$..name` spread.
    pub spread: bool,
    /// The byte range of the name in the template, without the sigil and
    /// without the annotation (`$x?` has just `x`).
    pub name_span: Range<usize>,
}

/// Every named parameter and spread occurrence of `sql`, in text order;
/// `None` when the template does not lex.
///
/// Native `$N` placeholders are excluded: they are not named parameters.
pub fn template_params(sql: &str) -> Option<Vec<TemplateParam>> {
    let output = crate::lexer::lex(sql).ok()?;
    let mut params: Vec<TemplateParam> = output
        .params
        .iter()
        .flat_map(|param| {
            param.name_spans.iter().map(|span| TemplateParam {
                name: param.name.clone(),
                spread: false,
                name_span: span.clone(),
            })
        })
        .collect();
    params.extend(output.spreads.iter().map(|spread| TemplateParam {
        name: spread.name.clone(),
        spread: true,
        name_span: spread.name_span.clone(),
    }));
    params.sort_by_key(|param| param.name_span.start);
    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(sql: &str) -> Vec<(String, bool)> {
        template_params(sql)
            .unwrap()
            .into_iter()
            .map(|param| (param.name, param.spread))
            .collect()
    }

    #[test]
    fn plain_params_are_in_text_order() {
        let sql = "SELECT $a, $b FROM t WHERE id = $c";
        let params = template_params(sql).unwrap();
        assert_eq!(
            names(sql),
            [
                ("a".into(), false),
                ("b".into(), false),
                ("c".into(), false)
            ]
        );
        for param in &params {
            assert_eq!(&sql[param.name_span.clone()], param.name);
        }
    }

    #[test]
    fn a_repeated_param_has_one_entry_per_occurrence() {
        let sql = "SELECT $x, $x FROM t";
        let params = template_params(sql).unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(&sql[params[0].name_span.clone()], "x");
        assert_eq!(&sql[params[1].name_span.clone()], "x");
        assert_ne!(params[0].name_span, params[1].name_span);
    }

    #[test]
    fn an_annotation_is_not_part_of_the_name_span() {
        for sql in [
            "SELECT $x? FROM t",
            "SELECT $x! FROM t",
            "SELECT $x?, $y! FROM t",
        ] {
            for param in template_params(sql).unwrap() {
                assert_eq!(&sql[param.name_span.clone()], param.name, "{sql}");
            }
        }
    }

    #[test]
    fn spreads_carry_their_name_span() {
        let sql = "INSERT INTO t VALUES $..items { name, email? }";
        let params = template_params(sql).unwrap();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "items");
        assert!(params[0].spread);
        assert_eq!(&sql[params[0].name_span.clone()], "items");

        let sql = "INSERT INTO t VALUES $..items";
        let params = template_params(sql).unwrap();
        assert_eq!(&sql[params[0].name_span.clone()], "items");
    }

    #[test]
    fn params_outside_normal_context_are_not_reported() {
        let sql = "SELECT '$x', $$ $x $$, $tag$ $x $tag$, 1 /* $x */ -- $x\n\
                   FROM t WHERE id = $y";
        assert_eq!(names(sql), [("y".into(), false)]);
    }

    #[test]
    fn name_spans_are_byte_offsets_past_multibyte_characters() {
        let sql = "SELECT 'é', $café, $日本 FROM t";
        let params = template_params(sql).unwrap();
        assert_eq!(names(sql), [("café".into(), false), ("日本".into(), false)]);
        for param in &params {
            assert_eq!(&sql[param.name_span.clone()], param.name);
        }
    }

    #[test]
    fn a_template_that_does_not_lex_has_no_params() {
        assert_eq!(template_params("SELECT 'unclosed"), None);
        assert_eq!(template_params("SELECT /* unclosed"), None);
    }

    #[test]
    fn native_placeholders_are_excluded() {
        assert_eq!(template_params("SELECT $1, $2, $name").unwrap().len(), 1);
    }
}
