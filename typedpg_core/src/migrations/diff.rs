//! The unified diff in the drift message.
//!
//! The only diff this workspace renders is the drift message's, so it is
//! not worth a diff crate: `similar` alone made the WebAssembly module the
//! npm runtime embeds several times larger. This is the same Myers line
//! diff the TypeScript runner had before the helpers moved to
//! `typedpg_core`, and a unified formatter that follows GNU diff.

/// How many unchanged lines a hunk keeps on either side of its changes.
const CONTEXT: usize = 3;

/// How many search-frontier entries (four bytes each) the diff may keep to
/// walk the shortest path back. Keeping them costs O(D²) in the edit
/// distance; past the budget — the diff would be thousands of lines long —
/// the changed middle becomes one deletion followed by one insertion. That
/// is still a correct diff, just not a minimal one.
const FRONTIER_BUDGET: usize = 1 << 20;

/// A unified diff of `applied` (what the tracking table stored) against
/// `file` (the migration on disk now), three lines of context, under the
/// headers GNU diff writes. Equal texts have no diff at all: the result is
/// empty, header included.
pub(super) fn unified_diff(applied: &str, file: &str, file_label: &str) -> String {
    render(&diff(&split_lines(applied), &split_lines(file)), file_label)
}

/// `text`'s lines, each keeping its `'\n'` — so a missing final newline
/// stays a different line, as it is to GNU diff — and an empty text having
/// none.
fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// The side of a diff a line is on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Equal,
    Delete,
    Insert,
}

struct Op<'a> {
    tag: Tag,
    /// The line, with its terminating `'\n'` unless it is the last line of
    /// a text that does not end with one.
    text: &'a str,
}

/// The equal/delete/insert operations that turn `a` into `b`: Myers'
/// shortest edit script.
fn diff<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Op<'a>> {
    // A common prefix and suffix are equal in every shortest script;
    // skipping them keeps the search to the text that actually changed.
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let a_changed = &a[prefix..a.len() - suffix];
    let b_changed = &b[prefix..b.len() - suffix];

    let mut ops = Vec::with_capacity(a.len().max(b.len()));
    ops.extend(a[..prefix].iter().map(|&text| Op {
        tag: Tag::Equal,
        text,
    }));
    match shortest_script(a_changed, b_changed) {
        Some(changed) => ops.extend(changed),
        None => {
            ops.extend(a_changed.iter().map(|&text| Op {
                tag: Tag::Delete,
                text,
            }));
            ops.extend(b_changed.iter().map(|&text| Op {
                tag: Tag::Insert,
                text,
            }));
        }
    }
    ops.extend(a[a.len() - suffix..].iter().map(|&text| Op {
        tag: Tag::Equal,
        text,
    }));
    ops
}

/// The shortest edit script for `a` against `b`, or `None` when the search
/// would exceed [`FRONTIER_BUDGET`].
///
/// The search is Myers' greedy algorithm: after `d` edits, `v[k]` is the
/// furthest x on diagonal `k = x - y` a path has reached. The first `d`
/// whose frontier reaches the end is the edit distance; keeping every
/// frontier the search passed lets the path be walked back.
fn shortest_script<'a>(a: &[&'a str], b: &[&'a str]) -> Option<Vec<Op<'a>>> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = n + m;
    // `max + 1` leaves room for the `k + 1` the first round reads.
    let offset = max + 1;
    let mut v = vec![-1isize; (2 * max + 3) as usize];
    v[(offset + 1) as usize] = 0;
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut budget = FRONTIER_BUDGET;

    let mut found = None;
    'search: for d in 0..=max {
        // The frontier before the round's edits, trimmed to the diagonals
        // the round can reach, is what the walk back needs.
        let entries = (2 * d + 3) as usize;
        if entries > budget {
            return None;
        }
        budget -= entries;
        trace.push(v[(offset - d - 1) as usize..=(offset + d + 1) as usize].to_vec());

        let mut k = -d;
        while k <= d {
            let step_down =
                k == -d || (k != d && v[(offset + k - 1) as usize] < v[(offset + k + 1) as usize]);
            let mut x = if step_down {
                v[(offset + k + 1) as usize]
            } else {
                v[(offset + k - 1) as usize] + 1
            };
            let mut y = x - k;
            // Follow the snake: equal lines cost no edit.
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(offset + k) as usize] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'search;
            }
            k += 2;
        }
    }
    found?;

    // Walk the frontiers back from the end, collecting the lines the path
    // passed in reverse.
    let mut ops = Vec::with_capacity(max as usize);
    let (mut x, mut y) = (n, m);
    for depth in (0..trace.len()).rev() {
        let frontier = &trace[depth];
        let d = depth as isize;
        let at = |k: isize| frontier[(k + d + 1) as usize];
        let k = x - y;
        let step_down = k == -d || (k != d && at(k - 1) < at(k + 1));
        let prev_k = if step_down { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op {
                tag: Tag::Equal,
                text: a[(x - 1) as usize],
            });
            x -= 1;
            y -= 1;
        }
        if depth > 0 {
            if x == prev_x {
                ops.push(Op {
                    tag: Tag::Insert,
                    text: b[prev_y as usize],
                });
            } else {
                ops.push(Op {
                    tag: Tag::Delete,
                    text: a[prev_x as usize],
                });
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    Some(ops)
}

/// The unified-diff text for `ops` under `file_label`, or an empty string
/// when no line changes.
fn render(ops: &[Op<'_>], file_label: &str) -> String {
    // A hunk starts three unchanged lines before its first change and ends
    // three after its last; changes with more than that many unchanged
    // lines between them belong to separate hunks.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < ops.len() {
        while i < ops.len() && ops[i].tag == Tag::Equal {
            i += 1;
        }
        if i == ops.len() {
            break;
        }
        let first = i;
        let mut last = i;
        let mut unchanged = 0;
        i += 1;
        while i < ops.len() {
            if ops[i].tag == Tag::Equal {
                unchanged += 1;
                if unchanged > 2 * CONTEXT {
                    break;
                }
            } else {
                unchanged = 0;
                last = i;
            }
            i += 1;
        }

        let mut start = first;
        for _ in 0..CONTEXT {
            if start > 0 && ops[start - 1].tag == Tag::Equal {
                start -= 1;
            }
        }
        let mut end = last + 1;
        for _ in 0..CONTEXT {
            if end < ops.len() && ops[end].tag == Tag::Equal {
                end += 1;
            }
        }
        hunks.push((start, end));
    }

    // The lines consumed before each operation: a hunk's start line and its
    // counts follow from them.
    let mut old_before = Vec::with_capacity(ops.len() + 1);
    let mut new_before = Vec::with_capacity(ops.len() + 1);
    old_before.push(0);
    new_before.push(0);
    for op in ops {
        old_before.push(old_before[old_before.len() - 1] + usize::from(op.tag != Tag::Insert));
        new_before.push(new_before[new_before.len() - 1] + usize::from(op.tag != Tag::Delete));
    }

    if hunks.is_empty() {
        return String::new();
    }
    let mut out = String::from("--- applied\n+++ ");
    out.push_str(file_label);
    out.push('\n');
    for (start, end) in hunks {
        let old_count = old_before[end] - old_before[start];
        let new_count = new_before[end] - new_before[start];
        // An empty range names the line before it; a range of one line
        // carries no count.
        let old_start = old_before[start] + usize::from(old_count != 0);
        let new_start = new_before[start] + usize::from(new_count != 0);
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(old_start, old_count),
            range(new_start, new_count),
        ));
        for op in &ops[start..end] {
            out.push(match op.tag {
                Tag::Equal => ' ',
                Tag::Delete => '-',
                Tag::Insert => '+',
            });
            out.push_str(op.text);
            if !op.text.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    out
}

/// A hunk range as GNU diff writes it: `start,count`, with a single line
/// written as just its number.
fn range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

#[cfg(test)]
mod tests {
    use super::unified_diff;

    /// The expected strings below are what `similar` rendered before it was
    /// replaced, header and trailing newline included.
    fn unified(applied: &str, file: &str) -> String {
        unified_diff(applied, file, "0001_a.sql")
    }

    /// `line 1` … `line n`, newline-terminated.
    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect::<String>()
    }

    #[test]
    fn equal_texts_have_no_diff() {
        assert_eq!(unified("SELECT 1;\n", "SELECT 1;\n"), "");
        assert_eq!(unified("", ""), "");
    }

    #[test]
    fn an_insertion_shows_the_new_line_and_its_context() {
        assert_eq!(
            unified("a\nb\n", "a\nx\nb\n"),
            "--- applied\n+++ 0001_a.sql\n@@ -1,2 +1,3 @@\n a\n+x\n b\n"
        );
    }

    #[test]
    fn a_deletion_shows_the_old_line_and_its_context() {
        assert_eq!(
            unified("a\nb\nc\n", "a\nc\n"),
            "--- applied\n+++ 0001_a.sql\n@@ -1,3 +1,2 @@\n a\n-b\n c\n"
        );
    }

    #[test]
    fn a_change_keeps_three_lines_of_context() {
        let applied = numbered(10);
        assert_eq!(
            unified(&applied, &applied.replace("line 5\n", "LINE 5\n")),
            "--- applied\n+++ 0001_a.sql\n\
             @@ -2,7 +2,7 @@\n\
             \x20line 2\n\
             \x20line 3\n\
             \x20line 4\n\
             -line 5\n\
             +LINE 5\n\
             \x20line 6\n\
             \x20line 7\n\
             \x20line 8\n"
        );
    }

    /// Changes further apart than the two contexts are two hunks.
    #[test]
    fn far_apart_changes_are_separate_hunks() {
        let applied = numbered(20);
        let file = applied
            .replace("line 2\n", "LINE 2\n")
            .replace("line 19\n", "LINE 19\n");
        assert_eq!(
            unified(&applied, &file),
            "--- applied\n+++ 0001_a.sql\n\
             @@ -1,5 +1,5 @@\n\
             \x20line 1\n\
             -line 2\n\
             +LINE 2\n\
             \x20line 3\n\
             \x20line 4\n\
             \x20line 5\n\
             @@ -16,5 +16,5 @@\n\
             \x20line 16\n\
             \x20line 17\n\
             \x20line 18\n\
             -line 19\n\
             +LINE 19\n\
             \x20line 20\n"
        );
    }

    /// Seven unchanged lines leave one line between the two contexts: one
    /// line too many, so the changes split. Six keep one hunk.
    #[test]
    fn changes_close_enough_share_one_hunk() {
        let applied = numbered(20);
        let file = applied
            .replace("line 5\n", "LINE 5\n")
            .replace("line 12\n", "LINE 12\n");
        let diff = unified(&applied, &file);
        assert_eq!(diff.matches("@@ ").count(), 1);
        assert!(diff.contains("@@ -2,14 +2,14 @@\n"), "{diff}");

        let file = applied
            .replace("line 5\n", "LINE 5\n")
            .replace("line 13\n", "LINE 13\n");
        let diff = unified(&applied, &file);
        assert_eq!(diff.matches("@@ ").count(), 2);
        assert!(diff.contains("@@ -2,7 +2,7 @@\n"), "{diff}");
        assert!(diff.contains("@@ -10,7 +10,7 @@\n"), "{diff}");
    }

    #[test]
    fn an_empty_old_text_is_an_insertion_from_line_zero() {
        assert_eq!(
            unified("", "a\nb\n"),
            "--- applied\n+++ 0001_a.sql\n@@ -0,0 +1,2 @@\n+a\n+b\n"
        );
    }

    #[test]
    fn an_empty_new_text_is_a_deletion_to_line_zero() {
        assert_eq!(
            unified("a\nb\n", ""),
            "--- applied\n+++ 0001_a.sql\n@@ -1,2 +0,0 @@\n-a\n-b\n"
        );
    }

    /// A last line without its newline is different from the same line
    /// with one, and the marker follows the line it affects — context
    /// lines included — as GNU diff writes it.
    #[test]
    fn a_missing_final_newline_is_marked() {
        assert_eq!(
            unified("a\nb", "a\nc"),
            "--- applied\n+++ 0001_a.sql\n@@ -1,2 +1,2 @@\n\
             \x20a\n\
             -b\n\\ No newline at end of file\n\
             +c\n\\ No newline at end of file\n"
        );
        assert_eq!(
            unified("a\nb", "a\nb\n"),
            "--- applied\n+++ 0001_a.sql\n@@ -1,2 +1,2 @@\n\
             \x20a\n\
             -b\n\\ No newline at end of file\n\
             +b\n"
        );
        assert_eq!(
            unified("a\nb\n", "a\nb"),
            "--- applied\n+++ 0001_a.sql\n@@ -1,2 +1,2 @@\n\
             \x20a\n\
             -b\n\
             +b\n\\ No newline at end of file\n"
        );
        assert_eq!(
            unified("a\nb\nc", "a\nX\nc"),
            "--- applied\n+++ 0001_a.sql\n@@ -1,3 +1,3 @@\n\
             \x20a\n\
             -b\n\
             +X\n\
             \x20c\n\\ No newline at end of file\n"
        );
    }

    /// A small change to a large file stays one small hunk, and the diff is
    /// the shortest one: the search costs O((N + M)·D), not O(N·M).
    #[test]
    fn a_small_change_to_a_large_file_is_one_hunk() {
        let applied: String = (1..=15_000).map(|i| format!("line {i}\n")).collect();
        let mut lines: Vec<String> = (1..=15_000).map(|i| format!("line {i}\n")).collect();
        lines[10_000] = "changed\n".to_owned();
        let file: String = lines.concat();
        assert_eq!(
            unified(&applied, &file),
            "--- applied\n+++ 0001_a.sql\n\
             @@ -9998,7 +9998,7 @@\n\
             \x20line 9998\n\
             \x20line 9999\n\
             \x20line 10000\n\
             -line 10001\n\
             +changed\n\
             \x20line 10002\n\
             \x20line 10003\n\
             \x20line 10004\n"
        );
    }
}
