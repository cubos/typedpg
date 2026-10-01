use super::*;

// ──────────────────────────────────────────────────────────────────────────
// Findings: dedup by signature, record a minimized example.
// ──────────────────────────────────────────────────────────────────────────

/// A stable signature for a divergence: kind + the message with the
/// query-specific `SQL:\n---\n…\n---\n` block stripped and content
/// placeholders normalized (quoted strings → `"_"`, digit runs → `N`), so
/// two findings with the same root cause but different triggering queries /
/// literal contents collapse to one. Twenty probes of `'<garbage>'::date`
/// are one missing validator, not twenty findings.
pub(crate) fn signature(div: &Divergence) -> String {
    // A nullability report goes on with the scenario, parameters and
    // seeding behind it: per-query detail, not part of the root cause.
    let message = match div.message.find("\nscenario: ") {
        Some(end) if div.kind == DivergenceKind::Nullability => &div.message[..end],
        _ => div.message.as_str(),
    };
    let div = &Divergence {
        kind: div.kind,
        message: message.to_owned(),
    };
    let stripped = match (div.message.find("SQL:\n---\n"), div.message.find("\n---\n")) {
        (Some(start), _) => {
            // Remove from "SQL:" up to and including the closing "---" line.
            let after = &div.message[start..];
            if let Some(end) = after.find("\n---\n") {
                let rest = &after[end + 5..];
                format!("{}{}", &div.message[..start], rest)
            } else {
                div.message.clone()
            }
        }
        _ => div.message.clone(),
    };
    format!("{:?}|{}", div.kind, collapse_content(&stripped))
}

/// Collapse `"quoted"` spans to `"_"` and digit runs to `N` — shared by the
/// dedup signature and the summary's family bucketing.
pub(crate) fn collapse_content(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_quote = false;
    let mut prev_digit = false;
    for c in line.chars() {
        if c == '"' {
            if !in_quote {
                out.push_str("\"_\"");
            }
            in_quote = !in_quote;
            continue;
        }
        if in_quote {
            continue;
        }
        if c.is_ascii_digit() {
            if !prev_digit {
                out.push('N');
            }
            prev_digit = true;
        } else {
            prev_digit = false;
            out.push(c);
        }
    }
    out
}

pub(crate) struct Finding {
    pub(crate) kind: DivergenceKind,
    pub(crate) example_sql: String,
    pub(crate) message: String,
    /// True when surfaced by the single-fault path (high signal — a genuine
    /// single-error divergence, not error-ordering noise).
    pub(crate) single_fault: bool,
}

impl Finding {
    /// A finding that is a bug whatever the query's fault count: a single
    /// fault's divergence, or unsound nullability (a valid query's rows
    /// can't be "error-ordering noise").
    pub(crate) fn high_signal(&self) -> bool {
        self.single_fault || self.kind == DivergenceKind::Nullability
    }
}

pub(crate) fn write_findings(out_dir: &str, findings: &BTreeMap<String, Finding>) {
    if findings.is_empty() {
        return;
    }
    if std::fs::create_dir_all(out_dir).is_err() {
        eprintln!("fuzz: could not create {out_dir}; skipping file output");
        return;
    }
    for (n, f) in findings.values().enumerate() {
        // High-signal findings (single-fault, or unsound nullability) get a
        // `high-` prefix so they sort first and are easy to triage;
        // ordering-prone multi-fault ones get `multi-`.
        let tier = if f.high_signal() { "high" } else { "multi" };
        let path = format!("{out_dir}/{tier}-{:?}-{n:03}.sql", f.kind);
        let body = format!(
            "-- divergence kind: {:?}{}\n-- {}\n--\n-- full report:\n{}\n\n{};\n",
            f.kind,
            if f.single_fault {
                " (single-fault, high signal)"
            } else if f.high_signal() {
                " (high signal)"
            } else {
                " (multi-fault — may be error-ordering, not a bug)"
            },
            f.message.lines().next().unwrap_or(""),
            f.message
                .lines()
                .map(|l| format!("-- {l}"))
                .collect::<Vec<_>>()
                .join("\n"),
            f.example_sql,
        );
        let _ = std::fs::write(&path, body);
    }
    eprintln!("\nfuzz: wrote {} finding(s) to {out_dir}/", findings.len());
}

/// Coarse "family" of a finding for triage: the PG-side message (for
/// `ErrorPrefix`, the expected prefix; otherwise the first line) with quoted
/// literals collapsed to `"_"` and digit runs to `N`, so findings that differ
/// only in identifiers / constants group together.
pub(crate) fn family(f: &Finding) -> String {
    let line = f
        .message
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("PG (expected prefix): "))
        .or_else(|| f.message.lines().next())
        .unwrap_or("");
    collapse_content(line)
}

pub(crate) fn print_summary(iters: u32, findings: &BTreeMap<String, Finding>) {
    let mut by_kind: BTreeMap<String, u32> = BTreeMap::new();
    let mut by_family: BTreeMap<String, u32> = BTreeMap::new();
    for f in findings.values() {
        *by_kind.entry(format!("{:?}", f.kind)).or_default() += 1;
        *by_family.entry(family(f)).or_default() += 1;
    }
    let high = findings.values().filter(|f| f.high_signal()).count();
    eprintln!("\n──── fuzz summary ────");
    eprintln!("iterations:        {iters}");
    eprintln!("unique divergences: {}", findings.len());
    eprintln!(
        "  high signal (single-fault or nullability): {high}   multi-fault (may be ordering): {}",
        findings.len() - high
    );
    for (kind, count) in &by_kind {
        eprintln!("  {kind:<24} {count}");
    }
    // Top families (most frequent root-cause messages) to guide triage.
    let mut fams: Vec<(&String, &u32)> = by_family.iter().collect();
    fams.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    eprintln!("── top families ──");
    for (fam, count) in fams.into_iter().take(20) {
        let fam = if fam.len() > 80 { &fam[..80] } else { fam };
        eprintln!("  {count:>4}  {fam}");
    }
    eprintln!("──────────────────────");
}
