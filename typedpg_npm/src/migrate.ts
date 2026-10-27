// The migration runner: the Rust runner's (`typedpg::migrate`) behavior,
// tracking table and drift hash, so the two (and `typedpg migrate` /
// `cargo typedpg migrate`) can manage the same database.
//
// Each migration is `NNNN_description.sql`, with an optional
// `NNNN_description.down.sql`. A run takes an advisory lock, applies each
// pending migration in a transaction (unless the runner's `useTransaction`
// is off or the file starts with `-- no-transaction`, where its statements
// run one at a time) and records it with its text and normalized hash. A
// changed statement later is drift; comments, whitespace and unquoted case
// are not (see `core.ts`, the Rust helper compiled to wasm).
//
// The I/O stays here, on the caller's driver: the pure helpers both runners
// share (splitting, normalization, names, order, messages) come from
// `typedpg_core` through `typedpg_wasm`.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

import { type Driver, type Executor, driverFor, unsupported } from "./adapters.js";
import {
  compareMigrationNames,
  driftMessage,
  hash,
  hashVersion,
  hasNoTransactionMarker,
  locatePosition,
  parseMigrationName,
  splitStatements,
  stripBom,
  validateTableName,
} from "./core.js";

export {
  compareVersions,
  hasNoTransactionMarker,
  locatePosition,
  splitStatements,
  stripBom,
} from "./core.js";

export interface Migration {
  /** The file stem: `0001_create_users`. */
  readonly name: string;
  /** The numeric prefix, which orders the migrations (as a number). */
  readonly version: string;
  readonly sql: string;
  readonly downSql: string | null;
  /** The file's first line is `-- no-transaction`. */
  readonly noTransaction: boolean;
  /** The down file's first line is `-- no-transaction`. */
  readonly downNoTransaction: boolean;
}

/** The runner's settings: those of `typedpg.config.json`'s `migrations`. */
export interface MigrationsConfig {
  /** The tracking table, `name` or `schema.name`. Default `public._migrations`. */
  table: string;
  /** The advisory lock's id. Default 713705. */
  lockId: number | bigint;
  /** Run each migration in a transaction. Default true. */
  useTransaction: boolean;
  /**
   * Abort a run when an applied migration's statements were edited since;
   * else warn. Comments, whitespace and unquoted case do not count. Default
   * true. `acceptMigration` records an intentional edit.
   */
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
  /** Applied, and its statements edited since (comments and whitespace are not). */
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
    if (name.endsWith(".down.sql")) {
      down.set(name.slice(0, -".down.sql".length), stripBom(readFileSync(path, "utf8")));
    }
    else if (name.endsWith(".sql")) up.push(name);
  }
  const migrations = up.map((file) => {
    const stem = file.slice(0, -".sql".length);
    return migration(
      stem,
      join(dir, file),
      stripBom(readFileSync(join(dir, file), "utf8")),
      down.get(stem) ?? null,
    );
  });
  return { migrations: sortByVersion(migrations), config: settings(config) };
}

function settings(config: Partial<MigrationsConfig>): MigrationsConfig {
  const s = { ...DEFAULTS, ...config };
  // Interpolated into every statement: the Rust runner's validation, so
  // both runners accept exactly the same names.
  const invalid = validateTableName(s.table);
  if (invalid !== null) throw new MigrationError(invalid);
  return s;
}

function migration(name: string, path: string, sql: string, downSql: string | null): Migration {
  // The Rust runner's parser, message included: the two runners must accept
  // and reject the same file names.
  const parsed = parseMigrationName(name, path);
  if ("error" in parsed) throw new MigrationError(parsed.error);
  return {
    name,
    version: parsed.version,
    sql,
    downSql,
    noTransaction: hasNoTransactionMarker(sql),
    downNoTransaction: downSql !== null && hasNoTransactionMarker(downSql),
  };
}

function sortByVersion(migrations: Migration[]): Migration[] {
  // By the prefix as a number, not as text: `10_x` is a later migration
  // than `2_x`, and the name settles the order of two with the same
  // prefix. The Rust runner's order, from the shared helper.
  return migrations.sort((a, b) => compareMigrationNames(a.name, b.name));
}

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
         name         TEXT PRIMARY KEY,
         applied_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
         sql_source   TEXT,
         hash         TEXT,
         hash_version INTEGER
       );
       ALTER TABLE ${config.table} ADD COLUMN IF NOT EXISTS sql_source TEXT;
       ALTER TABLE ${config.table} ADD COLUMN IF NOT EXISTS hash TEXT;
       ALTER TABLE ${config.table} ADD COLUMN IF NOT EXISTS hash_version INTEGER;`,
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

/** An applied migration's stored drift hash, as `migrate` reads it. */
interface AppliedMigration {
  hash: string | null;
  hashVersion: number | null;
}

async function appliedHashes(d: Driver, config: MigrationsConfig): Promise<Map<string, AppliedMigration>> {
  const { rows } = await d.typedpgQuery(`SELECT name, hash, hash_version::text FROM ${config.table}`, []);
  return new Map(
    rows.map(([name, h, v]) => [
      name!,
      { hash: h ?? null, hashVersion: v === null || v === undefined ? null : Number(v) },
    ]),
  );
}

/**
 * The one-time upgrade of the tracking table's stored hashes: every row
 * whose `hash_version` is not the current one gets the normalized hash of
 * its stored text, when it has one. Rows without `sql_source` stay unhashed
 * — they can never drift.
 */
async function backfillHashes(d: Driver, config: MigrationsConfig): Promise<void> {
  const { rows } = await d.typedpgQuery(
    `SELECT name, sql_source FROM ${config.table}
     WHERE hash_version IS DISTINCT FROM ($1::pg_catalog.int4) AND sql_source IS NOT NULL`,
    [String(hashVersion())],
  );
  for (const [name, sql] of rows) {
    if (sql === null || sql === undefined) continue;
    await d.typedpgQuery(
      `UPDATE ${config.table} SET hash = ($1::pg_catalog.text), hash_version = ($2::pg_catalog.int4)
       WHERE name = ($3::pg_catalog.text)`,
      [hash(sql), String(hashVersion()), name!],
    );
  }
}

/** The stored `sql_source` of a drifted migration, for its diff. */
async function storedSqlSource(d: Driver, config: MigrationsConfig, name: string): Promise<string | null> {
  const { rows } = await d.typedpgQuery(
    `SELECT sql_source FROM ${config.table} WHERE name = ($1::pg_catalog.text)`,
    [name],
  );
  return rows[0]?.[0] ?? null;
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
      await backfillHashes(d, config);
      const applied = await appliedHashes(d, config);
      // Drift is checked over every applied migration before anything new
      // runs: a pending migration can sort before an applied one.
      for (const m of set.migrations) {
        const stored = applied.get(m.name);
        if (
          !stored ||
          stored.hashVersion !== hashVersion() ||
          stored.hash === null ||
          stored.hash === hash(m.sql)
        ) {
          continue;
        }
        const storedSql = await storedSqlSource(d, config, m.name);
        const message = driftMessage(m.name, storedSql, m.sql, config.failOnDrift);
        if (config.failOnDrift) throw new MigrationError(message);
        warn(config, `warning: ${message}`);
      }
      const done: string[] = [];
      for (const m of set.migrations) {
        if (applied.has(m.name)) continue;
        const mHash = hash(m.sql);
        await runScript(d, config, m, m.sql, "apply", () =>
          d.typedpgQuery(
            `INSERT INTO ${config.table} (name, sql_source, hash, hash_version)
             VALUES (($1::pg_catalog.text), ($2::pg_catalog.text), ($3::pg_catalog.text), ($4::pg_catalog.int4))`,
            [m.name, m.sql, mHash, String(hashVersion())],
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
                    WHERE a.attrelid = c.oid AND a.attname = 'sql_source' AND NOT a.attisdropped),
            EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                    WHERE a.attrelid = c.oid AND a.attname = 'hash' AND NOT a.attisdropped),
            EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                    WHERE a.attrelid = c.oid AND a.attname = 'hash_version' AND NOT a.attisdropped)
     FROM (SELECT pg_catalog.to_regclass(($1::pg_catalog.text)) AS oid) AS c`,
    [config.table],
  );
  const [tableExists, hasSource, hasHash, hasHashVersion] = exists.rows[0]!;
  const applied = new Map<string, [Date, string | null]>();
  if (tableExists === "t") {
    const hashColumn = hasHash === "t" ? "hash" : "NULL::text";
    const versionColumn = hasHashVersion === "t" ? "hash_version::text" : "NULL::text";
    const { rows } = await d.typedpgQuery(
      `SELECT name, extract(epoch FROM applied_at)::text, ${hashColumn}, ${versionColumn}
       FROM ${config.table} ORDER BY name`,
      [],
    );
    for (const [name, at, h, v] of rows) {
      // Only a current-algorithm hash is read; an older row's text is
      // hashed in memory below.
      const current = v !== null && v !== undefined && Number(v) === hashVersion();
      applied.set(name!, [new Date(Number(at) * 1000), current ? (h ?? null) : null]);
    }
    // The stale rows' text is fetched (and hashed) here, never written
    // back: `migrationStatus` only reads.
    if (hasSource === "t") {
      const staleFilter =
        hasHashVersion === "t" ? " AND hash_version IS DISTINCT FROM ($1::pg_catalog.int4)" : "";
      const { rows } = await d.typedpgQuery(
        `SELECT name, sql_source FROM ${config.table} WHERE sql_source IS NOT NULL${staleFilter}`,
        hasHashVersion === "t" ? [String(hashVersion())] : [],
      );
      for (const [name, sql] of rows) {
        const info = applied.get(name!);
        if (info && sql !== null && sql !== undefined) info[1] = hash(sql);
      }
    }
  }
  return set.migrations.map((m) => {
    const info = applied.get(m.name);
    const drifted = info !== undefined && info[1] !== null && info[1] !== hash(m.sql);
    if (drifted) warn(config, `warning: migration '${m.name}' has been modified since it was applied`);
    return { name: m.name, applied: info !== undefined, appliedAt: info?.[0] ?? null, drifted };
  });
}

/**
 * Record an applied migration's edited file as applied, without running it:
 * the stored text and normalized hash become the file's, and `appliedAt`
 * and the database are untouched. For an intentional edit that does not
 * need to run again.
 */
export async function acceptMigration(executor: Executor, set: MigrationSet, name: string): Promise<void> {
  const { config } = set;
  return withSession(executor, (d) =>
    locked(d, config, async () => {
      const m = set.migrations.find((m) => m.name === name);
      if (!m) throw new MigrationError(`no migration file named '${name}'`);
      const applied = await appliedHashes(d, config);
      if (!applied.has(name)) {
        throw new MigrationError(`migration '${name}' has not been applied: nothing to accept`);
      }
      await d.typedpgQuery(
        `UPDATE ${config.table} SET sql_source = ($1::pg_catalog.text), hash = ($2::pg_catalog.text),
         hash_version = ($3::pg_catalog.int4) WHERE name = ($4::pg_catalog.text)`,
        [m.sql, hash(m.sql), String(hashVersion()), name],
      );
    }),
  );
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
