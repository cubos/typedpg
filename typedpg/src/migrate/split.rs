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

/// The statements of `sql`, each with its terminating `;` (the last one may
/// have none). Pieces holding only whitespace and comments are dropped.
pub(crate) fn split_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut state = StatementState::default();

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
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
                // `E'…'`: a string where backslash escapes.
                if word.eq_ignore_ascii_case("e") && bytes.get(i) == Some(&b'\'') {
                    i = skip_quoted(bytes, i, b'\'', true);
                }
                state.identifier(word);
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
    /// psql's `identifiers[4]`: the first letter of each of the first four
    /// identifiers when it is one of CREATE / OR / REPLACE / FUNCTION /
    /// PROCEDURE, `0` otherwise.
    identifiers: [u8; 4],
}

impl StatementState {
    fn identifier(&mut self, word: &str) {
        self.has_token = true;
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
        if creates_routine && self.paren_depth == 0 {
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

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit() || b == b'$'
}

/// The index past a quoted run opened at `open` (`'…'` or `"…"`, the quote
/// doubled to escape it; with `backslash`, `\` escapes the next byte too).
fn skip_quoted(bytes: &[u8], open: usize, quote: u8, backslash: bool) -> usize {
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

/// The index past a `/* … */` comment opened at `open`; they nest.
fn skip_block_comment(bytes: &[u8], open: usize) -> usize {
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

/// If `open` starts a dollar-quote delimiter (`$$`, `$tag$`), the index past
/// its closing delimiter; `None` for any other `$` (a `$1` placeholder).
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
    let close = bytes[body..]
        .windows(delimiter.len())
        .position(|w| w == delimiter)?;
    Some(body + close + delimiter.len())
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
