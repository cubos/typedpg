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
// are not (the machine, in wasm, compares the same normalization as the Rust
// runner).
//
// The orchestration is not repeated here: it is the sans-I/O state machine
// in `typedpg_core::migrations::machine`, compiled to wasm. `migrate`,
// `migrationStatus`, `acceptMigration` and `revertMigration` drive it on the
// caller's driver — a query here, a script there, a sleep for the lock's
// backoff — and map its result back to this package's types.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

import { type Driver, type Executor, driverFor, unsupported } from "./adapters.js";
import {
  type MachineAction,
  type MachineDbError,
  type MachineMigration,
  type MachineOutcome,
  type MachineReply,
  type MachineStep,
  compareMigrationNames,
  hasNoTransactionMarker,
  migratorFree,
  migratorNew,
  migratorStep,
  parseMigrationName,
  stripBom,
  validateTableName,
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

function warn(config: MigrationsConfig, message: string) {
  (config.onWarning ?? console.warn)(message);
}

/**
 * Drive the wasm machine on `d`: perform each step, answer it, until it
 * finishes. The machine owns the lock, the transactions and every message;
 * this only translates between its steps and the driver.
 *
 * A driver error is kept as the original object: the machine's
 * `Failure::Db` must rethrow it unchanged, and a formatted migration failure
 * carries it as its cause. The first unanswered error is the one, not the
 * last reply's — the machine's cleanup (a ROLLBACK, the advisory unlock) may
 * have gone through other steps in between.
 */
async function drive(d: Driver, set: MigrationSet, action: MachineAction): Promise<MachineOutcome> {
  const config = set.config;
  const migrations: MachineMigration[] = set.migrations.map((m) => ({
    name: m.name,
    sql: m.sql,
    downSql: m.downSql,
    noTransaction: m.noTransaction,
    downNoTransaction: m.downNoTransaction,
  }));
  const handle = migratorNew(
    action,
    {
      table: config.table,
      lockId: config.lockId,
      useTransaction: config.useTransaction,
      failOnDrift: config.failOnDrift,
    },
    migrations,
  );
  let pending: unknown;
  try {
    let reply: MachineReply = { kind: "start" };
    for (;;) {
      const step: MachineStep = migratorStep(handle, reply);
      switch (step.kind) {
        case "query":
          try {
            const { rows } = await d.typedpgQuery(step.sql, step.params);
            reply = { kind: "rows", rows };
          } catch (e) {
            if (pending === undefined) pending = e;
            reply = { kind: "dbError", error: dbErrorOf(e) };
          }
          break;
        case "execute":
          try {
            await d.typedpgSimple!(step.sql);
            reply = { kind: "done" };
          } catch (e) {
            if (pending === undefined) pending = e;
            reply = { kind: "dbError", error: dbErrorOf(e) };
          }
          break;
        case "sleep":
          await new Promise((resolve) => setTimeout(resolve, step.ms));
          reply = { kind: "done" };
          break;
        case "warn":
          warn(config, step.message);
          reply = { kind: "done" };
          break;
        case "finish":
          return step.outcome;
        case "fail":
          if (step.failure.kind === "db") {
            if (pending === undefined) {
              throw new MigrationError("typedpg: the migration run failed without a database error");
            }
            throw pending;
          }
          throw new MigrationError(
            step.failure.message,
            pending === undefined ? undefined : { cause: pending },
          );
      }
    }
  } finally {
    migratorFree(handle);
  }
}

/** The details `formatPgError` used to read, as the machine's `DbError`. */
function dbErrorOf(e: unknown): MachineDbError {
  const error = e as {
    message?: unknown;
    severity?: unknown;
    detail?: unknown;
    hint?: unknown;
    position?: unknown;
    internalPosition?: unknown;
    internalQuery?: unknown;
    code?: unknown;
  };
  return {
    code: typeof error.code === "string" ? error.code : null,
    message: typeof error.message === "string" ? error.message : String(e),
    severity: typeof error.severity === "string" ? error.severity : null,
    detail: typeof error.detail === "string" ? error.detail : null,
    hint: typeof error.hint === "string" ? error.hint : null,
    position: optionalInteger(error.position),
    internalPosition: optionalInteger(error.internalPosition),
    internalQuery: typeof error.internalQuery === "string" ? error.internalQuery : null,
  };
}

/** PG's positions arrive as strings from node-postgres and numbers from
 * postgres.js; an empty one means "no position", as before. */
function optionalInteger(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && value !== "") {
    const number = Number(value);
    if (Number.isFinite(number)) return number;
  }
  return null;
}

/**
 * Apply every pending migration, in order; returns the names applied. Pass
 * a pool or a client, not a transaction: each migration runs in its own.
 */
export async function migrate(executor: Executor, set: MigrationSet): Promise<string[]> {
  return withSession(executor, async (d) => {
    const outcome = await drive(d, set, { kind: "up" });
    if (outcome.kind !== "applied") throw new MigrationError("typedpg: unexpected migration outcome");
    return outcome.names;
  });
}

/** Every migration, applied or pending. Read-only: it creates nothing. */
export async function migrationStatus(executor: Executor, set: MigrationSet): Promise<MigrationStatus[]> {
  const outcome = await drive(driverFor(executor), set, { kind: "status" });
  if (outcome.kind !== "status") throw new MigrationError("typedpg: unexpected migration outcome");
  return outcome.rows.map((row) => ({
    name: row.name,
    applied: row.applied,
    appliedAt: row.appliedAt === null ? null : new Date(row.appliedAt),
    drifted: row.drifted,
  }));
}

/**
 * Record an applied migration's edited file as applied, without running it:
 * the stored text and normalized hash become the file's, and `appliedAt`
 * and the database are untouched. For an intentional edit that does not
 * need to run again.
 */
export async function acceptMigration(executor: Executor, set: MigrationSet, name: string): Promise<void> {
  return withSession(executor, async (d) => {
    const outcome = await drive(d, set, { kind: "accept", name });
    if (outcome.kind !== "accepted") throw new MigrationError("typedpg: unexpected migration outcome");
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
  return withSession(executor, async (d) => {
    const outcome = await drive(d, set, {
      kind: "revert",
      name: name ?? null,
      force: options.force ?? false,
    });
    if (outcome.kind !== "reverted") throw new MigrationError("typedpg: unexpected migration outcome");
    return outcome.name;
  });
}
