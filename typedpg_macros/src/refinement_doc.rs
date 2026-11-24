//! The doc comment on a generated output field saying what the analysis
//! knows of its values beyond their type: `/// One of "a", "b".`,
//! `/// Between 1 and 10.`, `/// From 2020-01-01 to 2030-12-31.`,
//! `/// Never infinite.` — and, of an array, of its elements. The same on
//! the fields of a generated record struct, of each record field. The
//! field's Rust type is never narrowed: the comment only documents.

use proc_macro2::TokenStream;
use quote::quote;
use typedpg_analyzer::{
    AnalyzedColumn, OrdKind, OrdPoint, OrdRange, RecordField, Refinement, Type,
};

/// The `#[doc]` attributes for `col`'s output field (none when nothing is
/// known).
pub(crate) fn field_doc(col: &AnalyzedColumn) -> TokenStream {
    let lines = doc_lines(col);
    quote! { #( #[doc = #lines] )* }
}

/// The `#[doc]` attributes for the field of a generated record struct
/// standing for `field` (none when nothing is known).
pub(crate) fn record_field_doc(field: &RecordField) -> TokenStream {
    let lines = value_lines(&field.ty, &field.refinement, &field.elem_refinement);
    quote! { #( #[doc = #lines] )* }
}

/// The doc comment's lines, each a sentence.
pub(crate) fn doc_lines(col: &AnalyzedColumn) -> Vec<String> {
    value_lines(&col.pg_type, &col.refinement, &col.elem_refinement)
}

/// The lines saying what a value of `ty` is known to be: `refinement`, and
/// of an array, `elements` of each element.
fn value_lines(ty: &Type, refinement: &Refinement, elements: &Refinement) -> Vec<String> {
    let mut lines: Vec<String> = facts(ty, refinement)
        .into_iter()
        .map(|f| format!("{}.", capitalized(&f)))
        .collect();
    if let Some(element) = array_element(ty) {
        lines.extend(
            facts(element, elements)
                .into_iter()
                .map(|f| format!("Each element is {f}.")),
        );
    }
    lines
}

/// What `refinement` says of a value of `ty`, as lowercase phrases.
fn facts(ty: &Type, refinement: &Refinement) -> Vec<String> {
    let inner = without_domains(ty);
    let mut out = Vec::new();
    if let Some(values) = &refinement.values {
        // Every value of the type says nothing (an enum's labels, both
        // booleans).
        let whole = match inner {
            Type::Enum { labels, .. } => values.len() >= labels.len(),
            Type::Basic { name, .. } if name == "bool" => values.len() >= 2,
            _ => false,
        };
        if !whole {
            let quoted = matches!(inner, Type::Enum { .. })
                || matches!(inner, Type::Basic { name, .. } if matches!(name.as_str(), "text" | "varchar" | "unknown"));
            let mut values: Vec<&String> = values.iter().collect();
            // Integers in numeric order.
            values.sort_by_key(|v| (v.parse::<i128>().ok(), (*v).clone()));
            let listed: Vec<String> = values
                .iter()
                .map(|v| {
                    if quoted {
                        format!("{v:?}")
                    } else {
                        (*v).clone()
                    }
                })
                .collect();
            out.push(match listed.as_slice() {
                [] => "never a value".to_owned(),
                [one] => format!("always {one}"),
                _ => format!("one of {}", listed.join(", ")),
            });
        }
    } else if let Some(range) = refinement.range {
        match (range.lo, range.hi) {
            (Some(lo), Some(hi)) if lo == hi => out.push(format!("always {lo}")),
            (Some(lo), Some(hi)) => out.push(format!("between {lo} and {hi}")),
            (Some(lo), None) => out.push(format!("at least {lo}")),
            (None, Some(hi)) => out.push(format!("at most {hi}")),
            (None, None) => {}
        }
    }
    let bounded = refinement.bounds.as_ref().and_then(bounds_phrase);
    let finite_by_bounds = refinement
        .bounds
        .is_some_and(|b| b.excludes_infinities() && b.lo.is_some() && b.hi.is_some());
    if let Some(b) = bounded {
        out.push(b);
    }
    if refinement.finite && may_be_infinite(inner) && !finite_by_bounds {
        out.push("never infinite".to_owned());
    }
    out
}

/// `from X to Y`, `at least X and less than Y`, `exactly X`, ….
fn bounds_phrase(b: &OrdRange) -> Option<String> {
    let point = |p: &OrdPoint| render_point(b.kind, p);
    match (&b.lo, &b.hi) {
        (Some(lo), Some(hi)) if lo.inclusive && hi.inclusive && lo.point == hi.point => {
            Some(format!("always {}", point(&lo.point)))
        }
        (Some(lo), Some(hi)) if lo.inclusive && hi.inclusive => {
            Some(format!("from {} to {}", point(&lo.point), point(&hi.point)))
        }
        (lo, hi) => {
            let lo = lo.map(|l| {
                let word = if l.inclusive {
                    "at least"
                } else {
                    "greater than"
                };
                format!("{word} {}", point(&l.point))
            });
            let hi = hi.map(|h| {
                let word = if h.inclusive { "at most" } else { "less than" };
                format!("{word} {}", point(&h.point))
            });
            let parts: Vec<String> = lo.into_iter().chain(hi).collect();
            (!parts.is_empty()).then(|| parts.join(" and "))
        }
    }
}

/// A point as PostgreSQL prints a value of the kind (`timestamptz` in
/// UTC).
fn render_point(kind: OrdKind, p: &OrdPoint) -> String {
    let numeric = kind == OrdKind::Numeric;
    match p {
        OrdPoint::NegInfinity if numeric => "-Infinity".to_owned(),
        OrdPoint::Infinity if numeric => "Infinity".to_owned(),
        OrdPoint::NegInfinity => "-infinity".to_owned(),
        OrdPoint::Infinity => "infinity".to_owned(),
        OrdPoint::NaN => "NaN".to_owned(),
        OrdPoint::Finite(n) => match (kind, n.to_integer()) {
            (OrdKind::Date, Some(days)) => civil_date(days),
            (OrdKind::Timestamp, Some(us)) => timestamp(us),
            (OrdKind::TimestampTz, Some(us)) => format!("{}+00", timestamp(us)),
            _ => n.to_string(),
        },
    }
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` of the microsecond `us` counted from
/// 2000-01-01 00:00.
fn timestamp(us: i128) -> String {
    const DAY: i128 = 86_400_000_000;
    let (days, of_day) = (us.div_euclid(DAY), us.rem_euclid(DAY));
    let secs = of_day / 1_000_000;
    let frac = of_day % 1_000_000;
    let date = civil_date(days);
    // A BC date's era goes last, as PG prints it.
    let (date, era) = match date.strip_suffix(" BC") {
        Some(d) => (d.to_owned(), " BC"),
        None => (date, ""),
    };
    let mut out = format!(
        "{date} {:02}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    );
    if frac != 0 {
        let digits = format!("{frac:06}");
        out.push('.');
        out.push_str(digits.trim_end_matches('0'));
    }
    out.push_str(era);
    out
}

/// `YYYY-MM-DD` (`… BC` before year 1) of the day `days` counted from
/// 2000-01-01, in the proleptic Gregorian calendar PG uses (Howard
/// Hinnant's `civil_from_days`).
fn civil_date(days: i128) -> String {
    // From 1970-01-01, which is 10957 days before 2000-01-01.
    let z = days + 10_957 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i128::from(m <= 2);
    if y <= 0 {
        format!("{:04}-{m:02}-{d:02} BC", 1 - y)
    } else {
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// Whether a value of `ty` may be an infinity: a date, a timestamp, an
/// interval, a float or a `numeric`.
fn may_be_infinite(ty: &Type) -> bool {
    matches!(ty, Type::Basic { schema, name, .. } if schema == "pg_catalog" && matches!(
        name.as_str(),
        "date" | "timestamp" | "timestamptz" | "interval" | "float4" | "float8" | "numeric"
    ))
}

fn without_domains(mut ty: &Type) -> &Type {
    while let Type::Domain { base, .. } = ty {
        ty = base;
    }
    ty
}

fn array_element(ty: &Type) -> Option<&Type> {
    match without_domains(ty) {
        Type::Array { element, .. } => Some(element),
        _ => None,
    }
}

fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use typedpg_analyzer::PgCatalog;

    fn catalog() -> PgCatalog {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
             CREATE TABLE t (kind text NOT NULL CHECK (kind IN ('a', 'b')),
                             n int NOT NULL CHECK (n BETWEEN 1 AND 100),
                             m mood NOT NULL, free text,
                             d date NOT NULL CHECK (d BETWEEN '2020-01-01' AND '2030-12-31'),
                             ts timestamp NOT NULL CHECK (ts > '1999-12-31 23:59:59.5'),
                             tz timestamptz NOT NULL CHECK (tz < '2000-01-01 12:00:00+00'),
                             p numeric NOT NULL CHECK (p > 0 AND p <= 99.5),
                             fin timestamptz NOT NULL CHECK (isfinite(fin)),
                             old date NOT NULL CHECK (old >= '0044-03-15 BC'));",
        )
        .unwrap();
        db
    }

    /// Each output column's doc lines.
    fn docs(sql: &str) -> Vec<(String, Vec<String>)> {
        let a = catalog().analyze(sql).unwrap();
        a.columns
            .iter()
            .map(|c| (c.name.clone(), doc_lines(c)))
            .collect()
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn values_ranges_bounds_and_finiteness_are_documented() {
        assert_eq!(
            docs(
                "SELECT kind, n, m, free, d, ts, tz, p, fin, old, n > 0 AS pos, 7 AS seven,
                        m = 'ok' AS same
                 FROM t WHERE m IN ('ok', 'happy')"
            ),
            vec![
                ("kind".into(), lines(&[r#"One of "a", "b"."#])),
                ("n".into(), lines(&["Between 1 and 100."])),
                ("m".into(), lines(&[r#"One of "happy", "ok"."#])),
                ("free".into(), vec![]),
                ("d".into(), lines(&["From 2020-01-01 to 2030-12-31."])),
                ("ts".into(), lines(&["Greater than 1999-12-31 23:59:59.5."])),
                ("tz".into(), lines(&["Less than 2000-01-01 12:00:00+00."])),
                ("p".into(), lines(&["Greater than 0 and at most 99.5."])),
                ("fin".into(), lines(&["Never infinite."])),
                ("old".into(), lines(&["At least 0044-03-15 BC."])),
                // `n` is between 1 and 100: `n > 0` is decided.
                ("pos".into(), lines(&["Always true."])),
                ("seven".into(), lines(&["Always 7."])),
                ("same".into(), vec![]),
            ]
        );
    }

    #[test]
    fn array_elements_are_documented() {
        assert_eq!(
            docs("SELECT array_agg(kind) AS a, array_agg(d) AS b, array_agg(free) AS c FROM t"),
            vec![
                ("a".into(), lines(&[r#"Each element is one of "a", "b"."#])),
                (
                    "b".into(),
                    lines(&["Each element is from 2020-01-01 to 2030-12-31."])
                ),
                ("c".into(), vec![]),
            ]
        );
    }

    #[test]
    fn record_fields_are_documented() {
        let a = catalog()
            .analyze("SELECT ROW(kind, n, free, 'x', array_agg(kind)) AS r FROM t GROUP BY kind, n, free")
            .unwrap();
        let Type::AnonymousRecord { fields } = &a.columns[0].pg_type else {
            panic!("not a record: {:?}", a.columns[0].pg_type);
        };
        let docs: Vec<String> = fields
            .iter()
            .map(|f| record_field_doc(f).to_string())
            .collect();
        assert_eq!(
            docs,
            vec![
                r#"# [doc = "One of \"a\", \"b\"."]"#.to_owned(),
                r#"# [doc = "Between 1 and 100."]"#.to_owned(),
                String::new(),
                r#"# [doc = "Always \"x\"."]"#.to_owned(),
                r#"# [doc = "Each element is one of \"a\", \"b\"."]"#.to_owned(),
            ]
        );
    }

    #[test]
    fn the_field_carries_doc_attributes() {
        let a = catalog().analyze("SELECT n FROM t").unwrap();
        assert_eq!(
            field_doc(&a.columns[0]).to_string(),
            r#"# [doc = "Between 1 and 100."]"#
        );
        let a = catalog().analyze("SELECT free FROM t").unwrap();
        assert!(field_doc(&a.columns[0]).is_empty());
    }

    #[test]
    fn dates_print_in_the_proleptic_gregorian_calendar() {
        assert_eq!(civil_date(0), "2000-01-01");
        assert_eq!(civil_date(-1), "1999-12-31");
        assert_eq!(civil_date(60), "2000-03-01");
        assert_eq!(civil_date(-730_119), "0001-01-01");
        assert_eq!(civil_date(-730_120), "0001-12-31 BC");
        assert_eq!(timestamp(-1), "1999-12-31 23:59:59.999999");
        assert_eq!(
            timestamp(-730_120 * 86_400_000_000),
            "0001-12-31 00:00:00 BC"
        );
    }
}
