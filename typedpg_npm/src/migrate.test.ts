import assert from "node:assert/strict";
import { test } from "node:test";

import type { Driver, RawResult } from "./adapters.js";
import { MigrationError, embedMigrations, migrate } from "./migrate.js";

test("migrations are ordered by their numeric prefix", () => {
  // The prefix is a number, not text: `10_c` is a later migration than
  // `2_b`.
  const set = embedMigrations([
    ["10_c", "SELECT 1;", null],
    ["2_b", "SELECT 1;", null],
    ["1_a", "SELECT 1;", null],
    ["20_d", "SELECT 1;", null],
  ]);
  assert.deepEqual(
    set.migrations.map((m) => m.name),
    ["1_a", "2_b", "10_c", "20_d"],
  );
});

test("embedded migrations lose their BOM, as the Rust runner's and loadMigrations' do", () => {
  const [m] = embedMigrations([
    ["1_a", "\ufeff-- no-transaction\nCREATE INDEX CONCURRENTLY i ON a (id);", "\ufeffDROP INDEX i;"],
  ]).migrations;
  assert.equal(m!.sql, "-- no-transaction\nCREATE INDEX CONCURRENTLY i ON a (id);");
  assert.equal(m!.downSql, "DROP INDEX i;");
  assert.equal(m!.noTransaction, true);
  // Only a leading one: elsewhere it is text.
  assert.equal(embedMigrations([["1_a", "SELECT '\ufeff';", null]]).migrations[0]!.sql, "SELECT '\ufeff';");
});

test("a lock id must be a bigint's value, taken exactly", () => {
  const lock = (lockId: number | bigint) => embedMigrations([], { lockId }).config.lockId;
  assert.equal(lock(9007199254740991), 9007199254740991);
  assert.equal(lock(-(2n ** 63n)), -(2n ** 63n));
  // Rounded already, or out of range (it would wrap): another lock.
  assert.throws(() => lock(2 ** 53), MigrationError);
  assert.throws(() => lock(1.5), MigrationError);
  assert.throws(() => lock(2n ** 63n), MigrationError);
});

test("two migrations of the same version keep a deterministic order", () => {
  // The name settles it, so the runner and the compiler agree.
  const set = embedMigrations([
    ["1_b", "SELECT 1;", null],
    ["1_a", "SELECT 1;", null],
  ]);
  assert.deepEqual(
    set.migrations.map((m) => m.name),
    ["1_a", "1_b"],
  );
});

test("a version of any length is compared as a number", () => {
  const huge = `${"9".repeat(40)}_x`;
  const set = embedMigrations([
    [huge, "SELECT 1;", null],
    ["10_x", "SELECT 1;", null],
  ]);
  assert.deepEqual(
    set.migrations.map((m) => m.name),
    ["10_x", huge],
  );
});

/**
 * The machine's queries, by a stable part of their text: the real SQL is
 * long, and the test cares about the order of the steps, not its wording.
 * Anything else (`BEGIN`, `COMMIT`, a migration's own SQL) is its own label.
 */
function stepLabel(sql: string): string {
  if (sql.includes("pg_try_advisory_lock")) return "lock";
  if (sql.includes("pg_advisory_unlock")) return "unlock";
  if (sql.includes("c.oid IS NOT NULL")) return "table columns";
  if (sql.includes("CREATE TABLE IF NOT EXISTS")) return "ensure table";
  if (sql.includes("hash_version IS DISTINCT FROM")) return "backfill";
  if (sql.startsWith("SELECT name::pg_catalog.text, hash::pg_catalog.text")) return "applied";
  if (sql.startsWith("SELECT sql_source::pg_catalog.text FROM")) return "stored sql";
  if (sql.startsWith("INSERT INTO")) return "insert";
  return sql;
}

/** A `Driver` that answers the machine's steps with `answer` and records them. */
class ScriptedDriver implements Driver {
  /** Each step: a query's label, or the SQL an execute ran. */
  readonly log: string[] = [];

  constructor(
    private readonly answer: (sql: string, params: (string | null)[]) => (string | null)[][],
    private readonly execute: (sql: string) => void = () => {},
  ) {}

  async typedpgQuery(sql: string, params: (string | null)[]): Promise<RawResult> {
    this.log.push(stepLabel(sql));
    return { rows: this.answer(sql, params), rowCount: 0 };
  }

  async typedpgSimple(sql: string): Promise<void> {
    this.log.push(stepLabel(sql));
    this.execute(sql);
  }

  async typedpgSession(): Promise<{ driver: Driver; release(): Promise<void> }> {
    return { driver: this, release: async () => {} };
  }
}

test("a fresh run applies each pending migration in its own transaction", async () => {
  const set = embedMigrations([
    ["0001_a", "CREATE TABLE a (id INT);", null],
    ["0002_b", "CREATE TABLE b (id INT);", null],
  ]);
  const driver = new ScriptedDriver((sql) => (sql.includes("advisory") ? [["t"]] : []));

  assert.deepEqual(await migrate(driver, set), ["0001_a", "0002_b"]);
  assert.deepEqual(driver.log, [
    "lock",
    "table columns",
    "ensure table",
    "backfill",
    "applied",
    "BEGIN",
    "CREATE TABLE a (id INT);",
    "insert",
    "COMMIT",
    "BEGIN",
    "CREATE TABLE b (id INT);",
    "insert",
    "COMMIT",
    "unlock",
  ]);
});

test("a drifted migration fails with the diff and how to accept it", async () => {
  const stored = "CREATE TABLE a (\n    id INT\n);";
  const file = "CREATE TABLE a (\n    id BIGINT\n);";
  const set = embedMigrations([["0001_a", file, null]]);
  const driver = new ScriptedDriver((sql) => {
    if (sql.includes("advisory")) return [["t"]];
    if (sql.startsWith("SELECT name::pg_catalog.text, hash::pg_catalog.text")) {
      // A stored hash that is not the file's, under the current
      // normalization (`hash_version` is 2).
      return [["0001_a", "not-the-file's-hash", "2"]];
    }
    if (sql.startsWith("SELECT sql_source::pg_catalog.text FROM")) return [[stored]];
    return [];
  });

  await assert.rejects(migrate(driver, set), (e: unknown) => {
    assert.ok(e instanceof MigrationError);
    assert.match(e.message, /^migration '0001_a' has been modified since it was applied:\n/);
    assert.match(e.message, /^@@ -1,3 \+1,3 @@$/m);
    assert.match(e.message, /typedpg migrate accept 0001_a/);
    return true;
  });
  assert.ok(!driver.log.includes("BEGIN"), "a migration ran despite the drift");
  assert.equal(driver.log.at(-1), "unlock");
});

const LF_FUNCTION = "CREATE FUNCTION f() RETURNS text AS $$\nBEGIN\n  RETURN 'a\nb';\nEND\n$$ LANGUAGE plpgsql;\n";
const CRLF_FUNCTION = LF_FUNCTION.replaceAll("\n", "\r\n");
// LF_FUNCTION's hash as typedpg 0.6 to 0.8 stored it (`hash_version` 1).
const LF_FUNCTION_V1_HASH = "36008b21f609949ea9d09c40c9aad1d0";

test("rows of the previous hash version report no false drift", async () => {
  // A version-1 row with its text: the backfill re-hashes it with the
  // current algorithm, so a CRLF checkout of the LF file is no drift.
  const updates: (string | null)[][] = [];
  const upgraded = new ScriptedDriver((sql, params) => {
    if (sql.includes("advisory")) return [["t"]];
    if (sql.includes("hash_version IS DISTINCT FROM")) return [["0001_f", LF_FUNCTION]];
    if (sql.startsWith("UPDATE") && sql.includes("SET hash")) {
      updates.push(params);
      return [];
    }
    if (sql.startsWith("SELECT name::pg_catalog.text, hash::pg_catalog.text")) {
      return [["0001_f", updates[0]![0]!, updates[0]![1]!]];
    }
    return [];
  });
  assert.deepEqual(await migrate(upgraded, embedMigrations([["0001_f", CRLF_FUNCTION, null]])), []);
  assert.equal(updates.length, 1);
  assert.equal(updates[0]![1], "2");

  // A version-1 row without its text keeps its hash, compared with version
  // 1's algorithm: the hash the previous release stored is no drift, an
  // edit is.
  const unhashed = () =>
    new ScriptedDriver((sql) => {
      if (sql.includes("advisory")) return [["t"]];
      if (sql.startsWith("SELECT name::pg_catalog.text, hash::pg_catalog.text")) {
        return [["0001_f", LF_FUNCTION_V1_HASH, "1"]];
      }
      if (sql.startsWith("SELECT sql_source::pg_catalog.text FROM")) return [[null]];
      return [];
    });
  assert.deepEqual(await migrate(unhashed(), embedMigrations([["0001_f", LF_FUNCTION, null]])), []);
  await assert.rejects(
    migrate(unhashed(), embedMigrations([["0001_f", LF_FUNCTION.replace("'a", "'z"), null]])),
    /migration '0001_f' has been modified since it was applied/,
  );
});

test("a database error in a migration is rethrown with its line and as the cause", async () => {
  const sql = "CREATE TABLE ok_t (a int);\nCREATE TABL bad (a int);\n";
  const set = embedMigrations([["0001_bad", sql, null]]);
  const original = Object.assign(new Error('syntax error at or near "TABL"'), {
    code: "42601",
    severity: "ERROR",
    position: 35,
  });
  const driver = new ScriptedDriver(
    (query) => (query.includes("advisory") ? [["t"]] : []),
    (executed) => {
      if (executed === sql) throw original;
    },
  );

  await assert.rejects(migrate(driver, set), (e: unknown) => {
    assert.ok(e instanceof MigrationError);
    assert.equal(
      e.message,
      'failed to apply migration 0001_bad: ERROR: syntax error at or near "TABL"\n' +
        "LINE 2: CREATE TABL bad (a int);\n" +
        "               ^",
    );
    assert.equal(e.cause, original);
    return true;
  });
});

// node-postgres and postgres.js both expose the CONTEXT field as `where`:
// the line of a `DO` block that failed, as psql shows it.
test("a database error's context is part of the failure", async () => {
  const sql = "DO $$ BEGIN RAISE EXCEPTION 'boom'; END $$;";
  const set = embedMigrations([["0001_do", sql, null]]);
  const original = Object.assign(new Error("boom"), {
    code: "P0001",
    severity: "ERROR",
    where: "PL/pgSQL function inline_code_block line 1 at RAISE",
  });
  const driver = new ScriptedDriver(
    (query) => (query.includes("advisory") ? [["t"]] : []),
    (executed) => {
      if (executed === sql) throw original;
    },
  );

  await assert.rejects(migrate(driver, set), (e: unknown) => {
    assert.ok(e instanceof MigrationError);
    assert.equal(
      e.message,
      "failed to apply migration 0001_do: ERROR: boom\nCONTEXT: PL/pgSQL function inline_code_block line 1 at RAISE",
    );
    return true;
  });
});
