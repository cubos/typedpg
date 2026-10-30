// The bits of SQL text the runtime needs outside the codecs.

/**
 * The SQL without what follows its last statement: a trailing `;`, and
 * trailing comments.
 *
 * PostgreSQL rejects a `;` inside a subquery, and a trailing `--` comment
 * would swallow the subquery's closing parenthesis — so a query that ends
 * with either cannot be embedded in one. A `;` ends the statement, so
 * everything after it is junk too.
 *
 * The mirror of `typedpg_analyzer::strip_trailing_junk`.
 */
export function stripTrailingJunk(sql: string): string {
  let i = 0;
  // The end of the last *code* token: what follows it is whitespace, a
  // comment, or a `;` — the statement's terminator, not part of it.
  let codeEnd = 0;
  while (i < sql.length) {
    const c = sql[i]!;
    let step: number;
    let isCode = true;
    if (c === "'") {
      step = skipString(sql, i);
    } else if (c === '"') {
      step = skipQuotedIdentifier(sql, i);
    } else if (c === "-" && sql[i + 1] === "-") {
      step = skipLineComment(sql, i);
      isCode = false;
    } else if (c === "/" && sql[i + 1] === "*") {
      step = skipBlockComment(sql, i);
      isCode = false;
    } else if (c === "$") {
      step = dollarQuoteEnd(sql, i) ?? i + 1;
    } else if (c === ";") {
      // A `;` ends the statement: it is not part of the code.
      step = i + 1;
      isCode = false;
    } else if (isWhitespace(c)) {
      step = i + 1;
      isCode = false;
    } else {
      step = i + 1;
    }
    i = step;
    if (isCode) codeEnd = i;
  }
  return sql.slice(0, codeEnd).trimEnd();
}

function isWhitespace(c: string): boolean {
  return c === " " || c === "\t" || c === "\n" || c === "\r" || c === "\v" || c === "\f";
}

/** The index past the string literal opened at `open`. */
function skipString(sql: string, open: number): number {
  // `E'…'`: a string where a backslash escapes the next character.
  const escape =
    open >= 1 &&
    (sql[open - 1] === "E" || sql[open - 1] === "e") &&
    (open < 2 || !isIdentChar(sql[open - 2]!));
  let i = open + 1;
  while (i < sql.length) {
    if (sql[i] === "\\" && escape) {
      i += 2;
    } else if (sql[i] === "'") {
      // `''` is an escaped quote.
      if (sql[i + 1] === "'") i += 2;
      else return i + 1;
    } else {
      i += 1;
    }
  }
  return sql.length;
}

/** The index past the quoted identifier opened at `open`. */
function skipQuotedIdentifier(sql: string, open: number): number {
  let i = open + 1;
  while (i < sql.length) {
    if (sql[i] === '"') {
      // `""` is an escaped quote.
      if (sql[i + 1] === '"') i += 2;
      else return i + 1;
    } else {
      i += 1;
    }
  }
  return sql.length;
}

/** The index past the line comment opened at `open`, its line break included. */
function skipLineComment(sql: string, open: number): number {
  let i = open + 2;
  // scan.l's `non_newline` is `[^\n\r]`: a lone CR ends the comment too.
  while (i < sql.length && sql[i] !== "\n" && sql[i] !== "\r") i++;
  return i < sql.length ? i + 1 : i;
}

/** The index past the (nested) block comment opened at `open`. */
function skipBlockComment(sql: string, open: number): number {
  let depth = 0;
  let i = open;
  while (i < sql.length) {
    if (sql[i] === "/" && sql[i + 1] === "*") {
      depth++;
      i += 2;
    } else if (sql[i] === "*" && sql[i + 1] === "/") {
      depth--;
      i += 2;
      if (depth === 0) return i;
    } else {
      i++;
    }
  }
  return sql.length;
}

/** The index past the dollar-quoted string opened at `open`, when `open` starts one. */
function dollarQuoteEnd(sql: string, open: number): number | undefined {
  let i = open + 1;
  if (i < sql.length && isIdentStart(sql[i]!)) {
    while (i < sql.length && isIdentChar(sql[i]!)) i++;
  }
  if (sql[i] !== "$") return undefined;
  const delimiter = sql.slice(open, i + 1);
  const close = sql.indexOf(delimiter, i + 1);
  return close < 0 ? undefined : close + delimiter.length;
}

/**
 * PG's identifier characters: a letter — ASCII or any high-bit one, PG's
 * `\200-\377` — or an underscore, plus a digit for the continuation.
 */
function isIdentStart(c: string): boolean {
  return (c >= "a" && c <= "z") || (c >= "A" && c <= "Z") || c === "_" || c >= "\u0080";
}

function isIdentChar(c: string): boolean {
  return isIdentStart(c) || (c >= "0" && c <= "9");
}
