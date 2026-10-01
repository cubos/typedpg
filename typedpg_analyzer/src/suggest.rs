//! "Did you mean ..." helper used by diagnostics.
//!
//! Given a misspelled identifier and a list of candidates, returns the
//! closest match within a tolerance threshold — or `None` if no candidate is
//! close enough to be a useful suggestion.

/// Find the candidate closest to `query` (see [`rank_similar`]).
pub fn suggest_similar<'a, I>(query: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    rank_similar(query, candidates).into_iter().next()
}

/// Every candidate close enough to `query` to suggest, best first.
///
/// Closeness is the optimal-string-alignment distance (Levenshtein plus
/// adjacent transpositions, so `lenght` is one edit from `length`), within
/// a threshold that scales with the length of `query` (see
/// [`max_distance_for`]) — or any distance when the candidate starts with
/// a `query` of three or more characters. Ties are broken by the longer common prefix
/// (typos tend to come late in a word), then lexicographically — never by
/// the candidates' order, which comes from hash maps and changes between
/// builds.
///
/// Case-insensitive: comparison happens on lowercase forms, but the returned
/// candidates keep their original casing. Duplicates and candidates equal
/// to `query` itself are dropped: suggesting what the user wrote is noise.
pub(crate) fn rank_similar<'a, I>(query: &str, candidates: I) -> Vec<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let q_lower: Vec<char> = query.to_lowercase().chars().collect();
    let threshold = max_distance_for(q_lower.len());

    let mut ranked: Vec<(usize, std::cmp::Reverse<usize>, &'a str)> = Vec::new();
    for cand in candidates {
        if cand.is_empty() || cand == query {
            continue;
        }
        let c_lower: Vec<char> = cand.to_lowercase().chars().collect();
        // A name the user cut short (`ema` for `email`) is a match too,
        // whatever the distance.
        let truncated = q_lower.len() >= 3 && c_lower.starts_with(&q_lower);
        // Cheap bound: the distance is at least the length difference.
        if !truncated && c_lower.len().abs_diff(q_lower.len()) > threshold {
            continue;
        }
        let d = osa_distance(&q_lower, &c_lower);
        if d <= threshold || truncated {
            let prefix = q_lower
                .iter()
                .zip(&c_lower)
                .take_while(|(a, b)| a == b)
                .count();
            ranked.push((d, std::cmp::Reverse(prefix), cand));
        }
    }
    ranked.sort();
    ranked.dedup_by(|a, b| a.2 == b.2);
    ranked.into_iter().map(|(_, _, c)| c).collect()
}

/// The largest distance worth suggesting for a name of `len` characters:
/// roughly a third of it, capped at 3. One- and two-character names get no
/// typo tolerance at all — every other short name is one edit away from
/// them, so a "suggestion" there is a coin toss (`id` → `ip`); only a
/// case-only difference (distance 0) is reported.
fn max_distance_for(len: usize) -> usize {
    match len {
        0..=2 => 0,
        3..=5 => 1,
        6..=8 => 2,
        _ => 3,
    }
}

/// Optimal string alignment distance: insertions, deletions,
/// substitutions and transpositions of adjacent characters each cost 1
/// (a substring is never edited twice).
fn osa_distance(a: &[char], b: &[char]) -> usize {
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // Three rows: the one before the previous is needed for transpositions.
    let mut before: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];
    for i in 0..a.len() {
        curr[0] = i + 1;
        for j in 0..b.len() {
            let cost = usize::from(a[i] != b[j]);
            let mut d = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
            if i > 0 && j > 0 && a[i] == b[j - 1] && a[i - 1] == b[j] {
                d = d.min(before[j - 1] + 1);
            }
            curr[j + 1] = d;
        }
        std::mem::swap(&mut before, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osa(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        osa_distance(&a, &b)
    }

    #[test]
    fn distance_counts_a_transposition_once() {
        assert_eq!(osa("lenght", "length"), 1);
        assert_eq!(osa("lenght", "height"), 2);
        assert_eq!(osa("usres", "users"), 1);
        assert_eq!(osa("abc", "abc"), 0);
        assert_eq!(osa("", "abc"), 3);
        assert_eq!(osa("kitten", "sitting"), 3);
        // OSA, not full Damerau: "ca" → "abc" needs 3, not 2.
        assert_eq!(osa("ca", "abc"), 3);
    }

    #[test]
    fn typo_close_match() {
        let candidates = ["users", "posts", "comments"];
        assert_eq!(suggest_similar("userz", candidates), Some("users"));
        assert_eq!(suggest_similar("usres", candidates), Some("users"));
    }

    #[test]
    fn transposition_beats_two_substitutions() {
        // Plain Levenshtein has both at distance 2 and picked whichever the
        // hash map yielded first.
        assert_eq!(
            suggest_similar("lenght", ["height", "length"]),
            Some("length")
        );
        assert_eq!(
            suggest_similar("lenght", ["length", "height"]),
            Some("length")
        );
    }

    #[test]
    fn ties_are_broken_deterministically() {
        // Both one edit away; the longer common prefix wins, whatever the
        // candidates' order.
        assert_eq!(suggest_similar("nam", ["dam", "name"]), Some("name"));
        assert_eq!(suggest_similar("nam", ["name", "dam"]), Some("name"));
        // Same distance and prefix: lexicographic.
        assert_eq!(suggest_similar("abcx", ["abcz", "abcy"]), Some("abcy"));
        assert_eq!(suggest_similar("abcx", ["abcy", "abcz"]), Some("abcy"));
    }

    #[test]
    fn case_insensitive() {
        let candidates = ["Users", "Posts"];
        assert_eq!(suggest_similar("user", candidates), Some("Users"));
        // A case-only difference is a suggestion even for short names.
        assert_eq!(suggest_similar("ID", ["id"]), Some("id"));
    }

    #[test]
    fn too_distant() {
        let candidates = ["users", "posts"];
        assert_eq!(suggest_similar("xyz", candidates), None);
    }

    #[test]
    fn short_names_get_no_typo_tolerance() {
        assert_eq!(suggest_similar("x", ["y"]), None);
        assert_eq!(suggest_similar("id", ["ip", "ix"]), None);
        assert_eq!(suggest_similar("xy", ["ab"]), None);
    }

    #[test]
    fn a_truncated_name_matches_its_completion() {
        assert_eq!(suggest_similar("ema", ["email", "id"]), Some("email"));
        assert_eq!(suggest_similar("use", ["users", "user_id"]), Some("users"));
        // Not for one- or two-character prefixes.
        assert_eq!(suggest_similar("em", ["email"]), None);
    }

    #[test]
    fn the_query_itself_is_not_a_suggestion() {
        assert_eq!(suggest_similar("length", ["length"]), None);
        assert_eq!(
            rank_similar("lenth", ["length", "length", "lent"]),
            vec!["lent", "length"]
        );
    }

    #[test]
    fn picks_closest() {
        let candidates = ["users", "user_logs", "userdata"];
        // "userz" is distance 1 from "users", 4 from "user_logs", 4 from "userdata".
        assert_eq!(suggest_similar("userz", candidates), Some("users"));
    }
}
