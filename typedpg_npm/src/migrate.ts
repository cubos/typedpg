// The migration runner: the Rust runner's (`typedpg::migrate`) behavior,
// tracking table and drift hash, so the two (and `typedpg migrate` /
// `cargo typedpg migrate`) can manage the same database.
//
// Each migration is `NNNN_description.sql`, with an optional
// `NNNN_description.down.sql`. A run takes an advisory lock, applies each
// pending migration in a transaction (unless the runner's `useTransaction`
// is off or the file starts with `-- no-transaction`, where its statements
// run one at a time) and records it with its text, whose MD5 later detects
// an applied migration edited since.

import { createHash } from "node:crypto";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

import { type Driver, type Executor, driverFor, unsupported } from "./adapters.js";

export interface Migration {
  /** The file stem: `0001_create_users`. */
  readonly name: string;
  /** The numeric prefix, which orders the migrations (as text, as in Rust). */
  readonly version: string;
  readonly sql: string;
  readonly downSql: string | null;
  /** The file's first line is `-- no-transaction`. */
  readonly noTransaction: boolean;
}

/** The runner's settings: those of `typedpg.config.json`'s `migrations`. */
export interface MigrationsConfig {
  /** The tracking table, `name` or `schema.name`. Default `public._migrations`. */
  table: string;
  /** The advisory lock's id. Default 713705. */
  lockId: number | bigint;
  /** Run each migration in a transaction. Default true. */
  useTransaction: boolean;
  /** Abort a run when an applied migration was edited since; else warn. Default true. */
  failOnDrift: boolean;
  /** Where warnings go. Default `console.warn`. */
  onWarning?: (message: string) => void;
}

/** Migrations and the settings to run them with. */
export interface MigrationSet {
  readonly migrations: readonly Migration[];
  readonly config: MigrationsConfig;
}

export interface MigrationStatus {
  readonly name: string;
  readonly applied: boolean;
  readonly appliedAt: Date | null;
  /** Applied, and edited since. */
  readonly drifted: boolean;
}

export class MigrationError extends Error {
  override name = "MigrationError";
}

const DEFAULTS: MigrationsConfig = {
  table: "public._migrations",
  lockId: 713705,
  useTransaction: true,
  failOnDrift: true,
};

/** The migrations a generated module embeds (`"migrations": { "embed": true }`). */
export function embedMigrations(
  entries: readonly (readonly [name: string, sql: string, downSql: string | null])[],
  config: Partial<MigrationsConfig> = {},
): MigrationSet {
  const migrations = entries.map(([name, sql, downSql]) => migration(name, `${name}.sql`, sql, downSql));
  return { migrations: sortByVersion(migrations), config: settings(config) };
}

/**
 * The migrations in `dir` (none when it doesn't exist), as the Rust
 * runner's `MigrationSource::from_dir` reads them.
 */
export function loadMigrations(dir: string, config: Partial<MigrationsConfig> = {}): MigrationSet {
  let names: string[];
  try {
    names = readdirSync(dir).sort();
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return { migrations: [], config: settings(config) };
    throw e;
  }
  const down = new Map<string, string>();
  const up: string[] = [];
  for (const name of names) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) continue;
    if (name.endsWith(".down.sql")) down.set(name.slice(0, -".down.sql".length), readFileSync(path, "utf8"));
    else if (name.endsWith(".sql")) up.push(name);
  }
  const migrations = up.map((file) => {
    const stem = file.slice(0, -".sql".length);
    return migration(stem, join(dir, file), readFileSync(join(dir, file), "utf8"), down.get(stem) ?? null);
  });
  return { migrations: sortByVersion(migrations), config: settings(config) };
}

function settings(config: Partial<MigrationsConfig>): MigrationsConfig {
  const s = { ...DEFAULTS, ...config };
  // Interpolated into every statement: only a plain qualified identifier.
  if (!/^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)?$/.test(s.table)) {
    throw new MigrationError(
      `invalid migrations table "${s.table}": expected \`name\` or \`schema.name\` of letters, digits and _`,
    );
  }
  return s;
}

function migration(name: string, path: string, sql: string, downSql: string | null): Migration {
  const underscore = name.indexOf("_");
  if (underscore === -1) {
    throw new MigrationError(`migration file does not follow NNNN_description.sql format: ${path}`);
  }
  const version = name.slice(0, underscore);
  if (!/^\d+$/.test(version)) {
    throw new MigrationError(`migration file does not have a numeric prefix (NNNN_...): ${path}`);
  }
  if (underscore === name.length - 1) {
    throw new MigrationError(`migration file has no description after prefix: ${path}`);
  }
  const firstLine = sql.split("\n", 1)[0]!;
  return { name, version, sql, downSql, noTransaction: firstLine.trim() === "-- no-transaction" };
}

function sortByVersion(migrations: Migration[]): Migration[] {
  // Stable, by the prefix as text: the Rust runner's order.
  return migrations.sort((a, b) => (a.version < b.version ? -1 : a.version > b.version ? 1 : 0));
}

/** The MD5 of the migration's UTF-8 bytes; the server's `STORED_HASH` matches it. */
function hash(sql: string): string {
  return createHash("md5").update(sql, "utf8").digest("hex");
}

/** `md5(text)` would hash in the database encoding: compare UTF-8 bytes. */
const STORED_HASH = "md5(convert_to(sql_source, 'UTF8'))";

/** Run `f` on one connection of `executor`. */
async function withSession<T>(executor: Executor, f: (d: Driver) => Promise<T>): Promise<T> {
  const driver = driverFor(executor);
  if (!driver.typedpgSession || !driver.typedpgSimple) throw unsupported("typedpgSession and typedpgSimple (migrations)");
  const session = await driver.typedpgSession();
  try {
    return await f(session.driver);
  } finally {
    await session.release();
  }
}

/** Run `f` under the runner's advisory lock, the tracking table ensured. */
async function locked<T>(d: Driver, config: MigrationsConfig, f: () => Promise<T>): Promise<T> {
  const lock = String(config.lockId);
  // Polled, not waited for in `pg_advisory_lock`: a session waiting inside
  // a statement is a transaction the holder's `CREATE INDEX CONCURRENTLY`
  // waits for in turn — a deadlock PG resolves by killing one runner.
  for (let delay = 20; ; delay = Math.min(delay * 2, 500)) {
    const { rows } = await d.typedpgQuery("SELECT pg_try_advisory_lock(($1::pg_catalog.int8))", [lock]);
    if (rows[0]![0] === "t") break;
    await new Promise((resolve) => setTimeout(resolve, delay));
  }
  let result: T;
  try {
    // Under the lock: two runners racing on a fresh database would
    // otherwise both find the table missing.
    await d.typedpgSimple!(
      `CREATE TABLE IF NOT EXISTS ${config.table} (
         name       TEXT PRIMARY KEY,
         applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
         sql_source TEXT
       );
       ALTER TABLE ${config.table} ADD COLUMN IF NOT EXISTS sql_source TEXT;`,
    );
    result = await f();
  } catch (e) {
    await d.typedpgQuery("SELECT pg_advisory_unlock(($1::pg_catalog.int8))", [lock]).catch((unlock) => {
      warn(config, `typedpg: failed to release advisory lock: ${unlock}`);
    });
    throw e;
  }
  await d.typedpgQuery("SELECT pg_advisory_unlock(($1::pg_catalog.int8))", [lock]);
  return result;
}

async function appliedHashes(d: Driver, config: MigrationsConfig): Promise<Map<string, string | null>> {
  const { rows } = await d.typedpgQuery(`SELECT name, ${STORED_HASH} FROM ${config.table}`, []);
  return new Map(rows.map(([name, h]) => [name!, h ?? null]));
}

function warn(config: MigrationsConfig, message: string) {
  (config.onWarning ?? console.warn)(message);
}

/**
 * Apply every pending migration, in order; returns the names applied. Pass
 * a pool or a client, not a transaction: each migration runs in its own.
 */
export async function migrate(executor: Executor, set: MigrationSet): Promise<string[]> {
  const { config } = set;
  return withSession(executor, (d) =>
    locked(d, config, async () => {
      const applied = await appliedHashes(d, config);
      // Drift is checked over every applied migration before anything new
      // runs: a pending migration can sort before an applied one.
      for (const m of set.migrations) {
        const stored = applied.get(m.name);
        if (stored && stored !== hash(m.sql)) {
          if (config.failOnDrift) {
            throw new MigrationError(
              `migration '${m.name}' has been modified since it was applied; set ` +
                `migrations.failOnDrift = false to downgrade to a warning`,
            );
          }
          warn(config, `warning: migration '${m.name}' has been modified since it was applied`);
        }
      }
      const done: string[] = [];
      for (const m of set.migrations) {
        if (applied.has(m.name)) continue;
        await runScript(d, config, m, m.sql, "apply", () =>
          d.typedpgQuery(
            `INSERT INTO ${config.table} (name, sql_source) VALUES (($1::pg_catalog.text), ($2::pg_catalog.text))`,
            [m.name, m.sql],
          ),
        );
        done.push(m.name);
      }
      return done;
    }),
  );
}

/**
 * Run a migration's (or its down's) SQL, then `record` it: both in one
 * transaction, or — outside one — each statement on its own, then the
 * record.
 */
async function runScript(
  d: Driver,
  config: MigrationsConfig,
  m: Migration,
  sql: string,
  verb: "apply" | "revert",
  record: () => Promise<unknown>,
) {
  if (config.useTransaction && !m.noTransaction) {
    await d.typedpgSimple!("BEGIN");
    try {
      try {
        await d.typedpgSimple!(sql);
      } catch (e) {
        throw new MigrationError(`failed to ${verb} migration ${m.name}: ${formatPgError(e, sql, 0)}`, { cause: e });
      }
      await record();
      await d.typedpgSimple!("COMMIT");
    } catch (e) {
      await d.typedpgSimple!("ROLLBACK").catch(() => {});
      throw e;
    }
    return;
  }
  const statements = splitStatements(sql);
  for (let i = 0; i < statements.length; i++) {
    const [at, statement] = statements[i]!;
    try {
      await d.typedpgSimple!(statement);
    } catch (e) {
      throw new MigrationError(
        `failed to ${verb} migration ${m.name} (statement ${i + 1}): ${formatPgError(e, sql, at)}`,
        { cause: e },
      );
    }
  }
  await record();
}

/** Every migration, applied or pending. Read-only: it creates nothing. */
export async function migrationStatus(executor: Executor, set: MigrationSet): Promise<MigrationStatus[]> {
  const { config } = set;
  const d = driverFor(executor);
  const exists = await d.typedpgQuery(
    `SELECT c.oid IS NOT NULL,
            EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                    WHERE a.attrelid = c.oid AND a.attname = 'sql_source' AND NOT a.attisdropped)
     FROM (SELECT pg_catalog.to_regclass(($1::pg_catalog.text)) AS oid) AS c`,
    [config.table],
  );
  const [tableExists, hasSource] = exists.rows[0]!;
  const applied = new Map<string, [Date, string | null]>();
  if (tableExists === "t") {
    const source = hasSource === "t" ? STORED_HASH : "NULL::text";
    const { rows } = await d.typedpgQuery(
      `SELECT name, extract(epoch FROM applied_at)::text, ${source} FROM ${config.table} ORDER BY name`,
      [],
    );
    for (const [name, at, h] of rows) applied.set(name!, [new Date(Number(at) * 1000), h ?? null]);
  }
  return set.migrations.map((m) => {
    const info = applied.get(m.name);
    const drifted = info !== undefined && info[1] !== null && info[1] !== hash(m.sql);
    if (drifted) warn(config, `warning: migration '${m.name}' has been modified since it was applied`);
    return { name: m.name, applied: info !== undefined, appliedAt: info?.[0] ?? null, drifted };
  });
}

/**
 * Revert migration `name` (the last applied one without it) with its down
 * file and forget it; with `force` and no down file, only forget it.
 * Returns the name reverted.
 */
export async function revertMigration(
  executor: Executor,
  set: MigrationSet,
  name?: string,
  options: { force?: boolean } = {},
): Promise<string> {
  const { config } = set;
  return withSession(executor, (d) =>
    locked(d, config, async () => {
      const applied = await appliedHashes(d, config);
      const target = name ?? [...set.migrations].reverse().find((m) => applied.has(m.name))?.name;
      if (target === undefined) throw new MigrationError("no applied migrations to revert");
      if (!applied.has(target)) throw new MigrationError(`migration '${target}' is not applied`);
      const m = set.migrations.find((m) => m.name === target);
      if (!m) throw new MigrationError(`migration '${target}' not found in source`);
      const forget = () =>
        d.typedpgQuery(`DELETE FROM ${config.table} WHERE name = ($1::pg_catalog.text)`, [target]);
      if (m.downSql === null) {
        if (!options.force) {
          throw new MigrationError(
            `migration '${target}' has no down file (${target}.down.sql). Use force to remove the ` +
              `record without running SQL.`,
          );
        }
        await forget();
        return target;
      }
      await runScript(d, config, m, m.downSql, "revert", forget);
      return target;
    }),
  );
}

/**
 * A server error with its details, and its position as psql shows it: the
 * line of `sql` and a caret. `base` is the offset in `sql` of the text that
 * was sent (one statement, outside a transaction).
 */
function formatPgError(e: unknown, sql: string, base: number): string {
  const err = e as {
    message?: string;
    severity?: string;
    detail?: string;
    hint?: string;
    position?: string | number;
    internalPosition?: string;
    internalQuery?: string;
  };
  let out = err.severity ? `${err.severity}: ${err.message}` : String(err.message ?? e);
  if (err.detail) out += `\nDETAIL: ${err.detail}`;
  if (err.hint) out += `\nHINT: ${err.hint}`;
  if (err.position !== undefined && err.position !== null && err.position !== "") {
    out += locatePosition(sql, base, Number(err.position));
  } else if (err.internalPosition) {
    out += `\nINTERNAL POSITION: ${err.internalPosition}\nQUERY: ${err.internalQuery}`;
  }
  return out;
}

/** PG's 1-based character `position` in `sql.slice(base)` as `LINE n:` and a caret. */
export function locatePosition(sql: string, base: number, position: number): string {
  const sent = [...sql.slice(base)];
  const at = base + sent.slice(0, Math.max(position - 1, 0)).join("").length;
  const lineStart = sql.lastIndexOf("\n", at - 1) + 1;
  const lineEnd = sql.indexOf("\n", at) === -1 ? sql.length : sql.indexOf("\n", at);
  const lineNo = sql.slice(0, at).split("\n").length;
  const column = [...sql.slice(lineStart, at)].length;
  const prefix = `LINE ${lineNo}: `;
  return `\n${prefix}${sql.slice(lineStart, lineEnd)}\n${" ".repeat(prefix.length + column)}^`;
}

/**
 * The statements of `sql`, with their offsets, for a migration outside a
 * transaction: PostgreSQL runs a multi-statement simple query in one
 * implicit transaction block, where `CREATE INDEX CONCURRENTLY` fails. A
 * port of the Rust runner's splitter, which follows psql's lexer: a `;`
 * ends a statement unless it is in a string, quoted identifier,
 * dollar-quoted body, comment, parentheses, or a `BEGIN ATOMIC … END`
 * routine body. Pieces with only whitespace and comments are dropped.
 */
export function splitStatements(sql: string): [number, string][] {
  const out: [number, string][] = [];
  let start = 0;
  let i = 0;
  let state = new StatementState();
  const n = sql.length;
  while (i < n) {
    const c = sql[i]!;
    if (c === "-" && sql[i + 1] === "-") {
      while (i < n && sql[i] !== "\n") i++;
      continue;
    }
    if (c === "/" && sql[i + 1] === "*") {
      i = skipBlockComment(sql, i);
      continue;
    }
    if (c === "'" || c === '"') {
      state.hasToken = true;
      i = skipQuoted(sql, i, c, false);
      continue;
    }
    if (c === "$") {
      state.hasToken = true;
      i = dollarQuoteEnd(sql, i) ?? i + 1;
      continue;
    }
    if (c === "(") state.parenDepth++;
    else if (c === ")") state.parenDepth = Math.max(state.parenDepth - 1, 0);
    else if (c === ";" && state.parenDepth === 0 && state.beginDepth === 0) {
      if (state.hasToken) out.push([start, sql.slice(start, i + 1)]);
      start = i + 1;
      state = new StatementState();
      i++;
      continue;
    } else if (isIdentStart(c)) {
      const begin = i;
      while (i < n && isIdentCont(sql[i]!)) i++;
      const word = sql.slice(begin, i);
      // `E'…'`: a string where backslash escapes.
      if (word.toLowerCase() === "e" && sql[i] === "'") i = skipQuoted(sql, i, "'", true);
      state.identifier(word);
      continue;
    }
    if (!/\s/.test(c)) state.hasToken = true;
    i++;
  }
  if (state.hasToken) out.push([start, sql.slice(start)]);
  return out;
}

class StatementState {
  hasToken = false;
  parenDepth = 0;
  beginDepth = 0;
  identifierCount = 0;
  /** psql's `identifiers[4]`: the first letter of each of the first four identifiers among its keywords. */
  identifiers = ["", "", "", ""];

  identifier(word: string) {
    this.hasToken = true;
    const w = word.toLowerCase();
    if (["create", "function", "procedure", "or", "replace"].includes(w) && this.identifierCount < 4) {
      this.identifiers[this.identifierCount] = w[0]!;
    }
    this.identifierCount++;
    const ids = this.identifiers;
    const createsRoutine =
      ids[0] === "c" &&
      (ids[1] === "f" || ids[1] === "p" || (ids[1] === "o" && ids[2] === "r" && (ids[3] === "f" || ids[3] === "p")));
    if (createsRoutine && this.parenDepth === 0) {
      if (w === "begin") this.beginDepth++;
      // CASE ends with END too; it only matters inside a BEGIN.
      else if (w === "case") {
        if (this.beginDepth >= 1) this.beginDepth++;
      } else if (w === "end") this.beginDepth = Math.max(this.beginDepth - 1, 0);
    }
  }
}

function isIdentStart(c: string): boolean {
  return /[A-Za-z_]/.test(c) || c.charCodeAt(0) >= 0x80;
}

function isIdentCont(c: string): boolean {
  return isIdentStart(c) || /[0-9$]/.test(c);
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

/** Past the closing delimiter of a dollar quote opened at `open`; `undefined` for any other `$`. */
function dollarQuoteEnd(sql: string, open: number): number | undefined {
  let i = open + 1;
  if (i < sql.length && isIdentStart(sql[i]!)) {
    while (i < sql.length && (isIdentStart(sql[i]!) || /[0-9]/.test(sql[i]!))) i++;
  }
  if (sql[i] !== "$") return undefined;
  const delimiter = sql.slice(open, i + 1);
  const close = sql.indexOf(delimiter, i + 1);
  return close === -1 ? undefined : close + delimiter.length;
}
