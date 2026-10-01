//! Pretty-printer for analyzer diagnostics.
//!
//! Renders a [`RawError`] into a multi-line diagnostic inspired by
//! [`miette`](https://github.com/zkat/miette): Unicode box-drawing
//! characters frame the snippet, underline markers (`─`) point at the
//! offending tokens, and connectors (`╰─`) attach labels.
//!
//! Example output:
//!
//! ```text
//! relation "userz" does not exist
//!   ╭────
//! 2 │ FROM userz
//!   ·      ──┬──
//!   ·        ╰─ relation does not exist
//!   ╰────
//!   help: did you mean "users"?
//! ```
//!
//! The first line is the PostgreSQL-verbatim message — `pg_sanity`'s prefix
//! check sees it unchanged. The line/column markers are encoded
//! geometrically (gutter line number + caret column) instead of being
//! spelled out in a header, so the snippet has no redundant text. Span
//! offsets are translated from the post-lex SQL back into the original SQL
//! via the lexer's offset map.

use crate::error::{RawError, SourceSpan};
use crate::param::LexOutput;

/// Render a `RawError` into the final multi-line diagnostic string.
pub(crate) fn render(
    pg_message: &str,
    raw: &RawError,
    sql_original: &str,
    lex_output: &LexOutput,
) -> String {
    let primary = raw.primary.as_ref().and_then(|l| {
        span_to_position(translate(l.span, lex_output), sql_original)
            .map(|p| (p, l.message.as_str()))
    });

    let secondaries: Vec<(Position, &str)> = raw
        .secondaries
        .iter()
        .filter_map(|l| {
            let s = translate(l.span, lex_output);
            span_to_position(s, sql_original).map(|p| (p, l.message.as_str()))
        })
        .collect();

    if primary.is_none() && secondaries.is_empty() {
        // No location info at all — keep it flat. Optional hint still surfaces.
        let mut out = format!("{pg_message}\n");
        push_trailer(&mut out, raw);
        return out;
    }

    // The first line carries the message; pg_sanity ignores anything past
    // the first '\n' anchor (it does a prefix check on the message before
    // any extra rendering), so we keep the message verbatim there.
    let mut out = String::new();
    out.push_str(pg_message);
    out.push('\n');

    // Group entries by line, marking primary vs secondary.
    let mut by_line: std::collections::BTreeMap<usize, Vec<MarkerEntry>> =
        std::collections::BTreeMap::new();
    if let Some((p, msg)) = &primary {
        by_line.entry(p.line).or_default().push(MarkerEntry {
            pos: p.clone(),
            label: (!msg.is_empty()).then_some((*msg).to_string()),
            is_primary: true,
        });
    }
    for (p, msg) in &secondaries {
        by_line.entry(p.line).or_default().push(MarkerEntry {
            pos: p.clone(),
            label: Some((*msg).to_string()),
            is_primary: false,
        });
    }

    let max_line = *by_line.keys().last().unwrap();
    let gutter = max_line.to_string().len();
    let gutter_pad = " ".repeat(gutter);

    out.push_str(&format!("{gutter_pad} ╭────\n"));

    let mut prev_line: Option<usize> = None;
    for (line_no, entries) in &by_line {
        // Gap between non-adjacent lines: insert an aux `·` row.
        if let Some(prev) = prev_line
            && *line_no > prev + 1
        {
            out.push_str(&format!("{gutter_pad} ·\n"));
        }
        prev_line = Some(*line_no);

        let line_text = nth_line(sql_original, *line_no);
        out.push_str(&format!(
            "{:>width$} │ {}\n",
            line_no,
            line_text,
            width = gutter
        ));

        render_markers_for_line(&mut out, &gutter_pad, entries);
    }

    out.push_str(&format!("{gutter_pad} ╰────\n"));
    push_trailer(&mut out, raw);
    out
}

/// Append the `help:` and `note:` lines that follow the snippet. A
/// multi-line note keeps its later lines aligned under its first.
pub(crate) fn push_trailer(out: &mut String, raw: &RawError) {
    if let Some(hint) = &raw.hint {
        out.push_str(&format!("  help: {hint}\n"));
    }
    for note in &raw.notes {
        out.push_str("  note: ");
        out.push_str(&note.replace('\n', "\n        "));
        out.push('\n');
    }
}

/// One marker on a source line — a position plus an optional label and a
/// primary/secondary flag (kept around for future styling differences).
#[derive(Clone)]
struct MarkerEntry {
    pos: Position,
    label: Option<String>,
    /// Kept for future styling differentiation (e.g. bold underline for
    /// primary vs lighter line for secondaries). Currently both use `─`.
    #[allow(dead_code)]
    is_primary: bool,
}

/// Render the underline + label lines for all markers that share a single
/// source line.
///
/// Each marker with a label gets a `┬` at its start column, joining the
/// underline above to a `╰─` connector + label on its own line below:
///
/// ```text
///   ·      ┬────
///   ·      ╰─ relation does not exist
/// ```
///
/// Multiple labels stack with `│` connectors so each one clearly attaches
/// to its underline:
///
/// ```text
///   ·    ┬──            ┬───
///   ·    │               ╰─ second label
///   ·    ╰─ first label
/// ```
fn render_markers_for_line(out: &mut String, gutter_pad: &str, entries: &[MarkerEntry]) {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|e| e.pos.col_start);

    let max_col = sorted.iter().map(|e| e.pos.col_end).max().unwrap_or(1);

    // Build the underline row: '─' across each marker's columns.
    let mut underline = vec![' '; max_col];
    for e in &sorted {
        for c in &mut underline[(e.pos.col_start - 1)..(e.pos.col_end - 1)] {
            *c = '─';
        }
    }
    // Then overlay a '┬' at (roughly) the middle column of every marker
    // that carries a label — that's the column the `╰─` connector will
    // descend from. We pick `start + length / 2`, which biases ties to the
    // right (so a 4-wide span gets ┬ at offset 2, not 1).
    for e in &sorted {
        if e.label.is_some() {
            let len = e.pos.col_end - e.pos.col_start;
            let mid = e.pos.col_start + len / 2;
            underline[mid - 1] = '┬';
        }
    }
    let underline_str: String = underline.iter().collect();
    let underline_trimmed = underline_str.trim_end().to_string();
    out.push_str(&format!("{gutter_pad} · {}\n", underline_trimmed));

    // Attach each label on its own line via a `╰─` connector. Render
    // labels right-to-left so vertical `│` connectors fall under markers
    // whose label hasn't been printed yet.
    let with_labels: Vec<&MarkerEntry> = sorted.iter().filter(|e| e.label.is_some()).collect();
    // Anchor column of each label: same midpoint formula as the underline's
    // `┬`, so the connector lines up perfectly.
    let anchor_col = |e: &MarkerEntry| {
        let len = e.pos.col_end - e.pos.col_start;
        e.pos.col_start + len / 2
    };
    for (idx, e) in with_labels.iter().enumerate().rev() {
        let col = anchor_col(e);
        let mut row = vec![' '; col - 1];
        // Vertical pipes for markers to the left whose labels haven't
        // appeared yet (they sit below this row).
        for left in &with_labels[..idx] {
            let lcol = anchor_col(left);
            if lcol >= 1 && lcol - 1 < row.len() {
                row[lcol - 1] = '│';
            }
        }
        let conn: String = row.iter().collect();
        let label = e.label.as_deref().unwrap_or("");
        out.push_str(&format!("{gutter_pad} · {}╰─ {}\n", conn, label));
    }
}

#[derive(Debug, Clone)]
struct Position {
    /// 1-based line number in the original SQL.
    line: usize,
    /// 1-based column where the span starts.
    col_start: usize,
    /// 1-based column where the span ends (exclusive).
    col_end: usize,
}

fn translate(span: SourceSpan, lex_output: &LexOutput) -> SourceSpan {
    let (s, e) = lex_output.original_span(span.start, span.end);
    SourceSpan::new(s, e)
}

/// Convert a byte span in the original SQL into a 1-based (line, col_start, col_end).
///
/// If the span crosses multiple lines, the position covers from `col_start`
/// on the start line to the end of that line — multi-line underlines aren't
/// rendered (they're rare and visually noisy).
fn span_to_position(span: SourceSpan, sql: &str) -> Option<Position> {
    if span.start > sql.len() {
        return None;
    }
    let start = span.start;
    let end = span.end.min(sql.len()).max(start + 1);

    let bytes = sql.as_bytes();
    let mut line = 1usize;
    let mut line_start = 0usize;
    for (i, &b) in bytes.iter().enumerate().take(start) {
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }

    // Compute col_end clamped to end-of-line.
    let line_end = bytes[line_start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| line_start + p)
        .unwrap_or(bytes.len());
    // At least one column wide: a span at the end of a line (`syntax error
    // at end of input`) still gets its marker, just past the last character.
    let end_on_line = end.min(line_end).max(start + 1);

    Some(Position {
        line,
        col_start: 1 + (start - line_start),
        col_end: 1 + (end_on_line - line_start),
    })
}

fn nth_line(sql: &str, line: usize) -> &str {
    sql.lines().nth(line.saturating_sub(1)).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{RawError, SourceSpan};
    use crate::lexer::lex;

    #[test]
    fn renders_single_primary_with_hint() {
        let sql = "SELECT *\nFROM userz\nWHERE id = 1";
        let lex_output = lex(sql).unwrap();
        let start = sql.find("userz").unwrap();
        let span = SourceSpan::new(start, start + "userz".len());
        let raw =
            RawError::undefined_table("userz", Some(span), Some("did you mean \"users\"?".into()));
        let rendered = render(&raw.pg_message(), &raw, sql, &lex_output);

        let expected = "\
relation \"userz\" does not exist
  ╭────
2 │ FROM userz
  ·      ──┬──
  ·        ╰─ relation does not exist
  ╰────
  help: did you mean \"users\"?
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn renders_notes_after_the_hint() {
        let sql = "SELECT f(1)";
        let lex_output = lex(sql).unwrap();
        let raw = RawError::undefined_function(
            "function f(integer) does not exist".into(),
            Some(SourceSpan::new(7, 8)),
            Some("No function matches.".into()),
        )
        .with_note("candidates are:\n  f(text)\n  f(bytea)");
        let rendered = render(&raw.pg_message(), &raw, sql, &lex_output);
        let expected = "\
function f(integer) does not exist
  ╭────
1 │ SELECT f(1)
  ·        ┬
  ·        ╰─ function does not exist
  ╰────
  help: No function matches.
  note: candidates are:
          f(text)
          f(bytea)
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn renders_without_span_is_flat() {
        let sql = "SELECT 1";
        let lex_output = lex(sql).unwrap();
        let raw = RawError::undefined_table("x", None, None);
        let rendered = render(&raw.pg_message(), &raw, sql, &lex_output);
        assert_eq!(rendered, "relation \"x\" does not exist\n");
    }

    #[test]
    fn renders_flat_with_hint_only() {
        let sql = "SELECT 1";
        let lex_output = lex(sql).unwrap();
        let raw = RawError::undefined_table("x", None, Some("did you mean \"users\"?".into()));
        let rendered = render(&raw.pg_message(), &raw, sql, &lex_output);
        assert_eq!(
            rendered,
            "relation \"x\" does not exist\n  help: did you mean \"users\"?\n"
        );
    }
}
