//! Split a migration into its statements, for migrations that run outside
//! a transaction.
//!
//! PostgreSQL runs every statement of a multi-statement simple query in one
//! implicit transaction block, so `CREATE INDEX CONCURRENTLY` (or anything
//! else that refuses a transaction block) fails there as soon as the text
//! holds a second statement. Outside a transaction the runner therefore
//! sends one statement at a time.
//!
//! The split follows psql's lexer (`psqlscan.l`): a `;` ends a statement
//! unless it sits inside a string (`'…'`, `E'…'`), a quoted identifier, a
//! dollar-quoted body, a comment (`--`, nested `/* */`), parentheses, or the
//! `BEGIN ATOMIC … END` body of a `CREATE [OR REPLACE] FUNCTION|PROCEDURE`,
//! which psql tracks with the same `BEGIN`/`CASE`/`END` depth heuristic used
//! here.
//!
//! Shared with `typedpg_wasm`, which the npm runtime calls: both runners
//! send the same statements, in the same order.

/// The statements of `sql`, each with its terminating `;` (the last one may
/// have none). Pieces holding only whitespace and comments are dropped.
pub fn split_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut state = StatementState::default();

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = skip_line_comment(bytes, i);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(bytes, i);
                continue;
            }
            b'\'' => {
                state.has_token = true;
                i = skip_quoted(bytes, i, b'\'', false);
                continue;
            }
            b'"' => {
                state.has_token = true;
                i = skip_quoted(bytes, i, b'"', false);
                continue;
            }
            b'$' => {
                state.has_token = true;
                i = match dollar_quote_end(bytes, i) {
                    Some(end) => end,
                    None => i + 1,
                };
                continue;
            }
            // A number, with an identifier touching it: psql lexes the two
            // as one (erroneous) token, `integer_junk` & co.
            b'0'..=b'9' => {
                state.has_token = true;
                i = skip_number(bytes, i);
                continue;
            }
            b'(' => state.paren_depth += 1,
            b')' => state.paren_depth = state.paren_depth.saturating_sub(1),
            b';' if state.paren_depth == 0 && state.begin_depth == 0 => {
                if state.has_token {
                    out.push(&sql[start..=i]);
                }
                start = i + 1;
                state = StatementState::default();
                i += 1;
                continue;
            }
            _ if is_ident_start(b) => {
                let begin = i;
                while i < bytes.len() && is_ident_cont(bytes[i]) {
                    i += 1;
                }
                let word = &sql[begin..i];
                state.has_token = true;
                let prefix = match word.as_bytes() {
                    [letter] => Some(letter.to_ascii_lowercase()),
                    _ => None,
                };
                match (prefix, bytes.get(i)) {
                    // `E'…'`: a string where backslash escapes.
                    (Some(b'e'), Some(b'\'')) => i = skip_quoted(bytes, i, b'\'', true),
                    // `B'…'`, `X'…'`, `N'…'`, and `U&'…'` / `U&"…"` (or the
                    // error token `U&`): the letter prefixes a literal, and
                    // is no identifier to psql.
                    (Some(b'b' | b'x' | b'n'), Some(b'\'')) | (Some(b'u'), Some(b'&')) => {}
                    _ => state.identifier(word),
                }
                continue;
            }
            _ => {}
        }
        if !b.is_ascii_whitespace() {
            state.has_token = true;
        }
        i += 1;
    }
    if state.has_token {
        out.push(&sql[start..]);
    }
    out
}

#[derive(Default)]
struct StatementState {
    has_token: bool,
    paren_depth: usize,
    begin_depth: usize,
    identifier_count: usize,
    /// psql's `init_idents`, as far as routines go: the first letter of
    /// each of the first four identifiers outside parentheses when it is
    /// one of CREATE / OR / REPLACE / FUNCTION / PROCEDURE, `0` otherwise.
    identifiers: [u8; 4],
}

impl StatementState {
    /// psql's `psqlscan_track_identifier`: an unquoted identifier or
    /// keyword, which only counts outside parentheses.
    fn identifier(&mut self, word: &str) {
        if self.paren_depth != 0 {
            return;
        }
        let is = |kw: &str| word.eq_ignore_ascii_case(kw);
        if ["create", "function", "procedure", "or", "replace"]
            .iter()
            .any(|kw| is(kw))
            && self.identifier_count < self.identifiers.len()
        {
            self.identifiers[self.identifier_count] = word.as_bytes()[0].to_ascii_lowercase();
        }
        self.identifier_count += 1;

        let ids = &self.identifiers;
        let creates_routine = ids[0] == b'c'
            && (ids[1] == b'f'
                || ids[1] == b'p'
                || (ids[1] == b'o' && ids[2] == b'r' && (ids[3] == b'f' || ids[3] == b'p')));
        if creates_routine {
            if is("begin") {
                self.begin_depth += 1;
            } else if is("case") {
                // CASE ends with END too; only matters inside a BEGIN.
                if self.begin_depth >= 1 {
                    self.begin_depth += 1;
                }
            } else if is("end") {
                self.begin_depth = self.begin_depth.saturating_sub(1);
            }
        }
    }
}

pub(crate) fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

pub(crate) fn is_ident_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit() || b == b'$'
}

/// The index past a quoted run opened at `open` (`'…'` or `"…"`, the quote
/// doubled to escape it; with `backslash`, `\` escapes the next byte too).
pub(crate) fn skip_quoted(bytes: &[u8], open: usize, quote: u8, backslash: bool) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        if backslash && bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// The index of the line break ending a `--` comment opened at `open`:
/// scan.l's `non_newline` is `[^\n\r]`, so a lone CR ends it too.
pub(crate) fn skip_line_comment(bytes: &[u8], open: usize) -> usize {
    let mut i = open;
    while i < bytes.len() && !matches!(bytes[i], b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// The index past a `/* … */` comment opened at `open`; they nest.
pub(crate) fn skip_block_comment(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0;
    let mut i = open;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// The index past the number starting at `open` (a digit): digits and `_`,
/// a fraction, an exponent, and an identifier touching it, which psql lexes
/// into the same token (`integer_junk`, `real_junk`, …): `1end` is no `END`.
fn skip_number(bytes: &[u8], open: usize) -> usize {
    let digits = |mut i: usize| {
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
            i += 1;
        }
        i
    };
    let mut i = digits(open);
    // `1.5`, `1.`, but not `1..5` (psql's `numericfail`).
    if bytes.get(i) == Some(&b'.') && bytes.get(i + 1) != Some(&b'.') {
        i = digits(i + 1);
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(i + 1), Some(b'+' | b'-')));
        if bytes.get(i + 1 + sign).is_some_and(u8::is_ascii_digit) {
            i = digits(i + 1 + sign);
        }
    }
    if i < bytes.len() && is_ident_start(bytes[i]) {
        while i < bytes.len() && is_ident_cont(bytes[i]) {
            i += 1;
        }
    }
    i
}

/// If `open` starts a dollar-quote delimiter (`$$`, `$tag$`), the index past
/// its closing delimiter — or the end of `bytes` when the body is
/// unterminated: psql sends the rest of the input as one statement, which
/// PG rejects whole, instead of running the statements the body swallowed.
/// `None` for any other `$` (a `$1` placeholder).
fn dollar_quote_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut i = open + 1;
    if i < bytes.len() && is_ident_start(bytes[i]) {
        while i < bytes.len() && (is_ident_start(bytes[i]) || bytes[i].is_ascii_digit()) {
            i += 1;
        }
    }
    if bytes.get(i) != Some(&b'$') {
        return None;
    }
    let delimiter = &bytes[open..=i];
    let body = i + 1;
    Some(
        match bytes[body..]
            .windows(delimiter.len())
            .position(|w| w == delimiter)
        {
            Some(close) => body + close + delimiter.len(),
            None => bytes.len(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::split_statements;

    #[track_caller]
    fn split(sql: &str) -> Vec<&str> {
        split_statements(sql).into_iter().map(str::trim).collect()
    }

    #[test]
    fn splits_on_top_level_semicolons() {
        assert_eq!(
            split("CREATE TABLE a (x int); CREATE INDEX ON a (x);\nSELECT 1"),
            [
                "CREATE TABLE a (x int);",
                "CREATE INDEX ON a (x);",
                "SELECT 1"
            ]
        );
    }

    #[test]
    fn drops_empty_and_comment_only_pieces() {
        assert_eq!(
            split("-- no-transaction\n;; SELECT 1; -- trailing; comment\n/* a; */"),
            ["SELECT 1;"]
        );
        assert!(split("  -- only a comment\n").is_empty());
    }

    #[test]
    fn ignores_semicolons_in_strings_identifiers_and_comments() {
        assert_eq!(
            split(
                "INSERT INTO \"t;1\" VALUES ('a;''b', E'c\\';d'); \
                 /* x; /* nested; */ y; */ SELECT 2;"
            ),
            [
                "INSERT INTO \"t;1\" VALUES ('a;''b', E'c\\';d');",
                "/* x; /* nested; */ y; */ SELECT 2;"
            ]
        );
    }

    // scan.l: `comment ("--"{non_newline}*)` with `non_newline [^\n\r]` — a
    // lone CR ends the comment (`printf 'select 1 -- c\rselect 2;' | psql`
    // is a syntax error at "2").
    #[test]
    fn a_carriage_return_ends_a_line_comment() {
        assert_eq!(split("select 1 -- c\r;select 2;").len(), 2);
        assert_eq!(
            split("select 1 -- c\r\n;select 2; -- d;\r\n"),
            ["select 1 -- c\r\n;", "select 2;"]
        );
    }

    #[test]
    fn ignores_semicolons_in_dollar_quotes_but_not_after_placeholders() {
        let body = "CREATE FUNCTION f() RETURNS int AS $fn$ BEGIN RETURN 1; END; $fn$ \
                    LANGUAGE plpgsql;";
        assert_eq!(
            split(&format!("{body} SELECT $$;$$;")),
            [body, "SELECT $$;$$;"]
        );
        assert_eq!(
            split("PREPARE p AS SELECT $1; SELECT 1"),
            ["PREPARE p AS SELECT $1;", "SELECT 1"]
        );
        // `$` inside an identifier starts no quote.
        assert_eq!(
            split("SELECT a$b$c; SELECT 1"),
            ["SELECT a$b$c;", "SELECT 1"]
        );
    }

    #[test]
    fn keeps_a_sql_standard_routine_body_whole() {
        let f = "CREATE OR REPLACE FUNCTION f(x int) RETURNS int LANGUAGE sql \
                 BEGIN ATOMIC SELECT CASE WHEN x > 0 THEN 1 ELSE 0 END; SELECT 2; END;";
        let p = "CREATE PROCEDURE p() BEGIN ATOMIC INSERT INTO t VALUES (1); END;";
        assert_eq!(
            split(&format!("{f}\n{p}\nBEGIN; COMMIT;")),
            [f, p, "BEGIN;", "COMMIT;"]
        );
    }

    // psql 19 (`psql -e -f`) sends everything after an unterminated
    // dollar quote as one statement, which PG rejects ("unterminated
    // dollar-quoted string"); splitting it would run the `DROP TABLE`.
    #[test]
    fn an_unterminated_dollar_quote_runs_to_the_end() {
        assert_eq!(
            split("CREATE TABLE t (x int); SELECT $a$ x; DROP TABLE t; SELECT 1;"),
            [
                "CREATE TABLE t (x int);",
                "SELECT $a$ x; DROP TABLE t; SELECT 1;"
            ]
        );
        // A `$` that opens no quote still splits.
        assert_eq!(split("SELECT $a x; SELECT 1"), ["SELECT $a x;", "SELECT 1"]);
    }

    // psql lexes a number and the identifier touching it as one token
    // (`integer_junk`, `real_junk`): `1end` ends no `BEGIN ATOMIC` block,
    // and psql 19 sends the function whole.
    #[test]
    fn an_identifier_touching_a_number_is_no_keyword() {
        let f = "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
                 BEGIN ATOMIC SELECT 1e5; SELECT 1end; END;";
        assert_eq!(split(&format!("{f} SELECT 3;")), [f, "SELECT 3;"]);
        let g = "CREATE FUNCTION g() RETURNS int LANGUAGE sql \
                 BEGIN ATOMIC SELECT 0x1F, 1_000.5e-3, $1end; END;";
        assert_eq!(split(&format!("{g} SELECT 4;")), [g, "SELECT 4;"]);
    }

    // What psql 19 sends for each (`psql -e -f`): a literal's prefix
    // letter (`x'…'`, `u&'…'`, `e'…'`) and an identifier inside
    // parentheses are no identifier to its CREATE FUNCTION heuristic.
    #[test]
    fn literal_prefixes_and_parenthesized_words_are_not_counted() {
        for (head, n) in [
            ("CREATE x'1' FUNCTION", 2),
            ("CREATE (a) FUNCTION", 3),
            ("CREATE u&'a' FUNCTION", 4),
            ("CREATE e'\\'' FUNCTION", 5),
            ("CREATE n'a' FUNCTION", 6),
        ] {
            let f = format!("{head} f() BEGIN ATOMIC SELECT 1; END;");
            let tail = format!("SELECT {n};");
            assert_eq!(split(&format!("{f} {tail}")), [f.as_str(), tail.as_str()]);
        }
    }

    #[test]
    fn parentheses_hold_semicolons_only_while_open() {
        assert_eq!(
            split("CREATE RULE r AS ON INSERT TO t DO (SELECT 1; SELECT 2); SELECT 3"),
            [
                "CREATE RULE r AS ON INSERT TO t DO (SELECT 1; SELECT 2);",
                "SELECT 3"
            ]
        );
    }
}
