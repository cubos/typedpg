// The normalized migration text drift detection compares: the mirror of the
// Rust runner's `typedpg::migrate::normalize`, so both runners see the same
// drift. Comments, whitespace and the case of unquoted words do not count;
// strings, quoted identifiers and dollar-quoted bodies are content and are
// copied verbatim. The scan follows psql's lexer, as `splitStatements` does.

import { createHash } from "node:crypto";

/**
 * The version of the normalization whose hashes the tracking table stores:
 * a row's `hash_version` names it, and `migrate` re-hashes rows of another
 * version from their `sql_source`.
 */
export const HASH_VERSION = 1;

/** The MD5 hex digest of {@link normalize}`(sql)`'s UTF-8 bytes. */
export function hash(sql: string): string {
  return createHash("md5").update(normalize(sql), "utf8").digest("hex");
}

/**
 * What a character would fuse with if a run between them disappeared:
 * touching two characters of one class makes one token, of different classes
 * (or of `"other"`) stays two.
 */
type Class = "word" | "operator" | "colon" | "other";

/**
 * `sql` normalized so that comments, whitespace and the case of unquoted
 * words do not count as drift, while any change to the statements does.
 * Unquoted ASCII letters are lowercased, as PostgreSQL folds them; strings,
 * quoted identifiers and dollar-quoted bodies are content and copied
 * verbatim.
 *
 * A maximal run of whitespace and/or comments becomes one ASCII space only
 * when the character just before it and the character just after it would
 * fuse into a single token if they touched — both are in the same class:
 *
 * - word class: ASCII letters, digits, `_`, `$`, `.`, any non-ASCII
 *   character, and the `'` or `"` that opens a literal. A whole verbatim
 *   literal counts as a word on both of its ends;
 * - operator class: PostgreSQL's operator characters — `+`, `-`, `*`, `/`,
 *   `<`, `>`, `=`, `~`, `!`, `@`, `#`, `%`, `^`, `&`, `|`, `` ` `` and `?`;
 * - colon class: `:` next to `:` (`::`).
 *
 * Any other pair — brackets, commas, semicolons, or a word next to an
 * operator or punctuation — drops the run entirely, comment or not: `t (a`,
 * `t(a)` and `t\n(\n  a` are the same text, as are `a - b` and `a-b`,
 * while `a b`, `- -1` and `: :` keep the boundary they spell, and a comment
 * between two word characters keeps the space (`a`, comment, `b` is `a b`,
 * not `ab`).
 *
 * Whitespace only matters where removing it would fuse two tokens; dropping
 * it everywhere else is what makes reformatting — indentation, line breaks
 * around punctuation — not drift. No two texts PostgreSQL lexes differently
 * normalize the same, except one accepted edge: a line break between two
 * string literals (`'a'\n'b'`) makes PostgreSQL concatenate them, while a
 * plain space (`'a' 'b'`) leaves two literals; both normalize alike.
 *
 * The `-- no-transaction` marker decides how the migration runs, so it is
 * part of the normalized text (the comment scan drops it otherwise).
 */
export function normalize(sql: string): string {
  let out = "";
  let last: Class | null = null;
  let pendingRun = false;
  let i = 0;
  const n = sql.length;

  /** Start a token of `c`, writing the one space the pending run needs. */
  const separate = (c: Class) => {
    if (pendingRun) {
      if (last === c && c !== "other") out += " ";
      pendingRun = false;
    }
    last = c;
  };
  // PostgreSQL folds ASCII letters only in unquoted words.
  const ordinary = (c: string) => {
    separate(classOf(c));
    out += c >= "A" && c <= "Z" ? String.fromCharCode(c.charCodeAt(0) + 32) : c;
  };

  while (i < n) {
    const c = sql[i]!;
    if (c === "-" && sql[i + 1] === "-") {
      while (i < n && sql[i] !== "\n") i++;
      pendingRun = true;
    } else if (c === "/" && sql[i + 1] === "*") {
      i = skipBlockComment(sql, i);
      pendingRun = true;
    } else if (isAsciiWhitespace(c)) {
      i++;
      pendingRun = true;
    } else if (c === "'") {
      // `E'…'`, where a backslash escapes the next character: the `E` only
      // prefixes the string when it is a word of its own.
      const escapes = i > 0 && /[Ee]/.test(sql[i - 1]!) && (i < 2 || !isIdentCont(sql[i - 2]!));
      const end = skipQuoted(sql, i, "'", escapes);
      separate("word");
      out += sql.slice(i, end);
      i = end;
    } else if (c === '"') {
      const end = skipQuoted(sql, i, '"', false);
      separate("word");
      out += sql.slice(i, end);
      i = end;
    } else if (c === "$") {
      const end = dollarQuoteSpan(sql, i);
      if (end === undefined) {
        // A `$1` parameter, or any other `$`: ordinary text.
        ordinary(c);
        i++;
      } else {
        separate("word");
        out += sql.slice(i, end);
        i = end;
      }
    } else if (isIdentStart(c)) {
      // A word, scanned whole: `$` inside an identifier starts no dollar
      // quote, and the `E` of `E'…'` is a word of its own.
      separate("word");
      while (i < n && isIdentCont(sql[i]!)) {
        const d = sql[i]!;
        out += d >= "A" && d <= "Z" ? String.fromCharCode(d.charCodeAt(0) + 32) : d;
        i++;
      }
    } else {
      ordinary(c);
      i++;
    }
  }

  return hasNoTransactionMarker(sql) ? `no-transaction\n${out}` : out;
}

/**
 * Whether the SQL's first line is the `-- no-transaction` marker: the
 * migration runs outside a transaction.
 *
 * The marker is the marker, not a word: a file edited on Windows may
 * start with a BOM, and the marker may carry a note after it. The mirror
 * of `typedpg_core::migration_marker::has_no_transaction_marker`.
 */
export function hasNoTransactionMarker(sql: string): boolean {
  const first = sql.split("\n", 1)[0]!.replace(/^\uFEFF/, "").trim();
  const rest = first.startsWith("-- no-transaction")
    ? first.slice("-- no-transaction".length)
    : null;
  if (rest === null) return false;
  if (rest === "") return true;
  const c = rest[0]!;
  return !/[A-Za-z0-9_]/.test(c);
}

function isAsciiWhitespace(c: string): boolean {
  // PostgreSQL's scanner: no vertical tab.
  return c === " " || c === "\t" || c === "\n" || c === "\r" || c === "\f";
}

function isIdentStart(c: string): boolean {
  return /[A-Za-z_]/.test(c) || c.charCodeAt(0) >= 0x80;
}

function isIdentCont(c: string): boolean {
  return isIdentStart(c) || /[0-9$]/.test(c);
}

/**
 * The class of the single character `c`: a letter, digit, `_`, `$`, `.`, a
 * non-ASCII character or the quote opening a literal is a word; the
 * characters PostgreSQL allows in operator names fuse as operators; `:`
 * only fuses with another `:`; everything else never fuses.
 */
function classOf(c: string): Class {
  if (
    isIdentStart(c) ||
    (c >= "0" && c <= "9") ||
    c === "$" ||
    c === "." ||
    c === "'" ||
    c === '"'
  ) {
    return "word";
  }
  if ("+-*/<>=~!@#%^&|`?".includes(c)) return "operator";
  return c === ":" ? "colon" : "other";
}

/** The index past a quoted run opened at `open` (doubled quote; with `backslash`, `\` escapes too). */
function skipQuoted(sql: string, open: number, quote: string, backslash: boolean): number {
  let i = open + 1;
  while (i < sql.length) {
    if (backslash && sql[i] === "\\") i += 2;
    else if (sql[i] === quote) {
      if (sql[i + 1] === quote) i += 2;
      else return i + 1;
    } else i++;
  }
  return sql.length;
}

/** The index past a `/* … *\/` comment opened at `open`; they nest. */
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
    } else i++;
  }
  return sql.length;
}

/**
 * The index past a dollar-quoted body opened at `open`; `undefined` for a `$`
 * that opens none (`$1`). An unterminated body copies the rest verbatim.
 */
function dollarQuoteSpan(sql: string, open: number): number | undefined {
  let i = open + 1;
  if (i < sql.length && isIdentStart(sql[i]!)) {
    while (i < sql.length && (isIdentStart(sql[i]!) || /[0-9]/.test(sql[i]!))) i++;
  }
  if (sql[i] !== "$") return undefined;
  const delimiter = sql.slice(open, i + 1);
  const close = sql.indexOf(delimiter, i + 1);
  return close === -1 ? sql.length : close + delimiter.length;
}

type Op = { kind: "equal" | "delete" | "insert"; line: string };

/**
 * A unified diff of `applied` (what the tracking table stored) against
 * `file` (the migration on disk now), with 3 lines of context. The Rust
 * runner renders the same format with `similar`; the classification is
 * Myers' O(ND) algorithm, so a small change to a large file stays cheap.
 */
export function unifiedDiff(applied: string, file: string, fileLabel: string): string {
  const ops = diffLines(splitLines(applied), splitLines(file));
  if (!ops.some((op) => op.kind !== "equal")) return "";

  const context = 3;
  // Changes further apart than the two contexts are separate hunks; each
  // hunk keeps 3 unchanged lines on either side of its first and last one.
  const hunks: [number, number][] = [];
  let i = 0;
  while (i < ops.length) {
    while (i < ops.length && ops[i]!.kind === "equal") i++;
    if (i >= ops.length) break;
    const firstChange = i;
    let lastChange = i;
    let equalRun = 0;
    for (i++; i < ops.length; i++) {
      if (ops[i]!.kind === "equal") {
        equalRun++;
        if (equalRun > 2 * context) break;
      } else {
        equalRun = 0;
        lastChange = i;
      }
    }
    let start = firstChange;
    for (let c = 0; c < context && start > 0 && ops[start - 1]!.kind === "equal"; c++) start--;
    let end = lastChange + 1;
    for (let c = 0; c < context && end < ops.length && ops[end]!.kind === "equal"; c++) end++;
    hunks.push([start, end]);
  }

  // Lines consumed before each op: a hunk's start line and counts.
  const oldBefore = new Array<number>(ops.length + 1).fill(0);
  const newBefore = new Array<number>(ops.length + 1).fill(0);
  for (let i = 0; i < ops.length; i++) {
    const op = ops[i]!;
    oldBefore[i + 1] = oldBefore[i]! + (op.kind === "insert" ? 0 : 1);
    newBefore[i + 1] = newBefore[i]! + (op.kind === "delete" ? 0 : 1);
  }

  const out = [`--- applied`, `+++ ${fileLabel}`];
  for (const [start, end] of hunks) {
    const oldCount = oldBefore[end]! - oldBefore[start]!;
    const newCount = newBefore[end]! - newBefore[start]!;
    const oldStart = oldCount === 0 ? oldBefore[start]! : oldBefore[start]! + 1;
    const newStart = newCount === 0 ? newBefore[start]! : newBefore[start]! + 1;
    out.push(`@@ -${range(oldStart, oldCount)} +${range(newStart, newCount)} @@`);
    for (let i = start; i < end; i++) {
      const op = ops[i]!;
      out.push(`${op.kind === "equal" ? " " : op.kind === "delete" ? "-" : "+"}${op.line}`);
    }
  }
  return out.join("\n");
}

/** A unified-diff range: a lone line has no count (`-3`). */
function range(start: number, count: number): string {
  return count === 1 ? `${start}` : `${start},${count}`;
}

/** `text`'s lines, without a final empty one for a trailing newline. */
function splitLines(text: string): string[] {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/**
 * The equal/delete/insert operations that turn `a` into `b`: Myers'
 * shortest edit script, keeping every frontier the search passed so the path
 * can be walked back.
 */
function diffLines(a: readonly string[], b: readonly string[]): Op[] {
  const n = a.length;
  const m = b.length;
  const max = n + m;
  const offset = max;
  const v = new Int32Array(2 * max + 2).fill(-1);
  v[offset + 1] = 0;
  const trace: Int32Array[] = [];
  let found = -1;

  for (let d = 0; d <= max && found < 0; d++) {
    trace.push(v.slice());
    for (let k = -d; k <= d; k += 2) {
      let x: number;
      if (k === -d || (k !== d && v[offset + k - 1]! < v[offset + k + 1]!)) {
        x = v[offset + k + 1]!;
      } else {
        x = v[offset + k - 1]! + 1;
      }
      let y = x - k;
      while (x < n && y < m && a[x] === b[y]) {
        x++;
        y++;
      }
      v[offset + k] = x;
      if (x >= n && y >= m) {
        found = d;
        break;
      }
    }
  }

  const ops: Op[] = [];
  let x = n;
  let y = m;
  for (let depth = trace.length - 1; depth >= 0; depth--) {
    const frontier = trace[depth]!;
    const k = x - y;
    const down =
      k === -depth || (k !== depth && frontier[offset + k - 1]! < frontier[offset + k + 1]!);
    const prevK = down ? k + 1 : k - 1;
    const prevX = frontier[offset + prevK]!;
    const prevY = prevX - prevK;
    while (x > prevX && y > prevY) {
      ops.push({ kind: "equal", line: a[x - 1]! });
      x--;
      y--;
    }
    if (depth > 0) {
      if (x === prevX) ops.push({ kind: "insert", line: b[prevY]! });
      else ops.push({ kind: "delete", line: a[prevX]! });
    }
    x = prevX;
    y = prevY;
  }
  ops.reverse();
  return ops;
}
