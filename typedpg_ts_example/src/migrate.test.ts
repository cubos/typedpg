// The migration runner on a live PostgreSQL: applying, reverting, drift,
// failures, concurrency — and that it shares its tracking table with the
// Rust runner behind `typedpg migrate` (TYPEDPG_BIN), each reading what the
// other applied.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, test } from "node:test";

import pg from "pg";
import postgres from "postgres";
import {
  MigrationError,
  acceptMigration,
  embedMigrations,
  loadMigrations,
  migrate,
  migrationStatus,
  revertMigration,
  type Executor,
  type MigrationSet,
} from "@cubos/typedpg";

import { migrations } from "./db.ts";
import { baseUrl, freshDatabase } from "./testdb.ts";

const bin = process.env.TYPEDPG_BIN;
const names = ["0001_init", "0002_types", "0003_things_index"];
const skip = !baseUrl && "DATABASE_URL is not set";

/** `typedpg migrate <args>` on `url`, from the example's directory. */
function cli(url: string, ...args: string[]): string {
  return execFileSync(bin!, ["migrate", ...args, "--url", url], { cwd: new URL("..", import.meta.url), encoding: "utf8" });
}

/** `set` with one more migration. */
function withExtra(set: MigrationSet, name: string, sql: string, down: string | null = null): MigrationSet {
  return embedMigrations(
    [...set.migrations.map((m) => [m.name, m.sql, m.downSql] as const), [name, sql, down]],
    set.config,
  );
}

async function withPool<T>(url: string, f: (pool: pg.Pool) => Promise<T>): Promise<T> {
  const pool = new pg.Pool({ connectionString: url, max: 4 });
  try {
    return await f(pool);
  } finally {
    await pool.end();
  }
}

describe("migrations", { skip }, () => {
  test("the embedded migrations are the directory's", () => {
    const fromDir = loadMigrations(new URL("../migrations", import.meta.url).pathname);
    assert.deepEqual(fromDir.migrations, migrations.migrations);
    assert.deepEqual(
      migrations.migrations.map((m) => [m.name, m.noTransaction, m.downSql !== null]),
      [
        ["0001_init", false, true],
        ["0002_types", false, true],
        ["0003_things_index", true, true],
      ],
    );
  });

  for (const [driver, open] of [
    ["pg", (url: string) => {
      const pool = new pg.Pool({ connectionString: url, max: 4 });
      return { db: pool as Executor, close: () => pool.end() };
    }],
    ["postgres.js", (url: string) => {
      const sql = postgres(url, { onnotice: () => {}, max: 4 });
      return { db: sql as Executor, close: () => sql.end() };
    }],
  ] as const) {
    test(`apply, status, revert and re-apply (${driver})`, async () => {
      const url = await freshDatabase(`typedpg_migrate_${driver.replace(/\W/g, "")}`, false);
      const { db, close } = open(url);
      try {
        assert.deepEqual(await migrate(db, migrations), names);
        assert.deepEqual(await migrate(db, migrations), []);
        const status = await migrationStatus(db, migrations);
        assert.deepEqual(status.map((s) => [s.name, s.applied, s.drifted]), names.map((n) => [n, true, false]));
        assert.ok(status.every((s) => s.appliedAt instanceof Date && Date.now() - s.appliedAt.getTime() < 60_000));
        // The no-transaction migration's down runs one statement at a time
        // (DROP INDEX CONCURRENTLY refuses a transaction block).
        assert.equal(await revertMigration(db, migrations), "0003_things_index");
        assert.deepEqual((await migrationStatus(db, migrations)).map((s) => s.applied), [true, true, false]);
        assert.deepEqual(await migrate(db, migrations), ["0003_things_index"]);
      } finally {
        await close();
      }
    });
  }

  test("the Rust runner reads what this one applied, and the other way around", { skip: !bin && "TYPEDPG_BIN is not set" }, async () => {
    const url = await freshDatabase("typedpg_migrate_interop", false);
    await withPool(url, async (pool) => {
      await migrate(pool, migrations);
      const status = cli(url, "status");
      for (const n of names) assert.match(status, new RegExp(`✓ ${n}\\s+\\(applied `));
      assert.doesNotMatch(status, /MODIFIED/);

      assert.match(cli(url, "down"), /Reverting 0003_things_index\.\.\. done/);
      assert.deepEqual((await migrationStatus(pool, migrations)).map((s) => s.applied), [true, true, false]);
      assert.match(cli(url, "up"), /Applied 1 migration/);
      assert.deepEqual(await migrate(pool, migrations), []);
    });
  });

  test("drift is a statement change; comments, whitespace and case are not", async () => {
    const url = await freshDatabase("typedpg_migrate_drift", false);
    await withPool(url, async (pool) => {
      await migrate(pool, migrations);
      const [first, ...rest] = migrations.migrations;
      const restEntries = rest.map((m) => [m.name, m.sql, m.downSql] as const);
      const commented = embedMigrations([
        [first!.name, `-- edited\n${first!.sql.replace("CREATE TABLE users (", "create table users\n(")}`, first!.downSql],
        ...restEntries,
      ]);
      // A comment, a line break and an unquoted word's case are not drift.
      assert.deepEqual(await migrate(pool, commented), []);
      assert.equal((await migrationStatus(pool, commented))[0]!.drifted, false);

      const edited = embedMigrations([
        [first!.name, first!.sql.replace("name text NOT NULL", "name text NOT NULL DEFAULT ''"), first!.downSql],
        ...restEntries,
        ["0004_more", "CREATE TABLE more (a int);", null],
      ]);
      await assert.rejects(migrate(pool, edited), (e: unknown) => {
        assert.ok(e instanceof MigrationError);
        assert.match(e.message, /migration '0001_init' has been modified since it was applied:/);
        assert.match(e.message, /--- applied\n\+\+\+ 0001_init\.sql\n@@ /);
        assert.match(e.message, /-    name text NOT NULL,/);
        assert.match(e.message, /\+    name text NOT NULL DEFAULT '',/);
        assert.match(e.message, /typedpg migrate accept 0001_init/);
        return true;
      });
      // Nothing ran: the check comes first.
      assert.equal((await migrationStatus(pool, edited)).find((s) => s.name === "0004_more")!.applied, false);
      const warnings: string[] = [];
      const lenient = embedMigrations(
        edited.migrations.map((m) => [m.name, m.sql, m.downSql] as const),
        { failOnDrift: false, onWarning: (w) => warnings.push(w) },
      );
      assert.deepEqual(await migrate(pool, lenient), ["0004_more"]);
      assert.equal(warnings.length, 1);
      assert.match(
        warnings[0]!,
        /^warning: migration '0001_init' has been modified since it was applied:[\s\S]*typedpg migrate accept 0001_init/,
      );
      assert.equal((await migrationStatus(pool, lenient))[0]!.drifted, true);

      // An intentional edit, accepted: the file becomes what is recorded,
      // and nothing runs.
      const before = (await pool.query("SELECT applied_at FROM public._migrations WHERE name = $1", ["0001_init"])).rows[0].applied_at;
      await acceptMigration(pool, lenient, "0001_init");
      assert.deepEqual(await migrate(pool, lenient), []);
      assert.equal((await migrationStatus(pool, lenient))[0]!.drifted, false);
      const row = (await pool.query("SELECT applied_at, sql_source FROM public._migrations WHERE name = $1", ["0001_init"])).rows[0];
      assert.deepEqual(row.applied_at, before);
      assert.equal(row.sql_source, lenient.migrations[0]!.sql);

      await assert.rejects(acceptMigration(pool, lenient, "0009_nope"), /no migration file named '0009_nope'/);
      const unapplied = withExtra(lenient, "0005_new", "CREATE TABLE new_t (a int);");
      await assert.rejects(
        acceptMigration(pool, unapplied, "0005_new"),
        /migration '0005_new' has not been applied: nothing to accept/,
      );
    });
  });

  test("a failing migration: rolled back in a transaction, partial without one", async () => {
    const url = await freshDatabase("typedpg_migrate_failure", false);
    await withPool(url, async (pool) => {
      await migrate(pool, migrations);
      const bad = withExtra(migrations, "0004_bad", "CREATE TABLE ok_t (a int);\nCREATE TABL bad (a int);\n");
      await assert.rejects(migrate(pool, bad), (e: unknown) => {
        assert.ok(e instanceof MigrationError);
        assert.equal(
          e.message,
          'failed to apply migration 0004_bad: ERROR: syntax error at or near "TABL"\n' +
            "LINE 2: CREATE TABL bad (a int);\n" +
            "               ^",
        );
        return true;
      });
      const exists = async (table: string) =>
        (await pool.query("SELECT to_regclass($1) IS NOT NULL AS e", [table])).rows[0].e as boolean;
      assert.equal(await exists("ok_t"), false);
      assert.equal((await migrationStatus(pool, bad)).at(-1)!.applied, false);

      const partial = withExtra(migrations, "0004_partial", "-- no-transaction\nCREATE TABLE ok2 (a int);\nCREATE TABL bad;\n");
      await assert.rejects(migrate(pool, partial), /failed to apply migration 0004_partial \(statement 2\): ERROR: syntax error at or near "TABL"\nLINE 3: CREATE TABL bad;\n {15}\^/);
      assert.equal(await exists("ok2"), true);
      assert.equal((await migrationStatus(pool, partial)).at(-1)!.applied, false);
    });
  });

  test("concurrent runners apply each migration once", async () => {
    const url = await freshDatabase("typedpg_migrate_concurrent", false);
    await withPool(url, async (pool) => {
      const runs = await Promise.all([migrate(pool, migrations), migrate(pool, migrations), migrate(pool, migrations)]);
      assert.deepEqual(runs.flat().sort(), names);
      const { rows } = await pool.query("SELECT count(*)::int AS n FROM public._migrations");
      assert.equal(rows[0].n, names.length);
    });
  });

  test("names and settings are validated as the Rust runner does", (t) => {
    const dir = mkdtempSync(join(tmpdir(), "typedpg-migrations-"));
    t.after(() => rmSync(dir, { recursive: true, force: true }));
    const at = (file: string) => {
      const d = mkdtempSync(join(dir, "case-"));
      writeFileSync(join(d, file), "SELECT 1;");
      return () => loadMigrations(d);
    };
    assert.throws(at("nounderscore.sql"), /does not follow NNNN_description\.sql format/);
    assert.throws(at("abc_x.sql"), /does not have a numeric prefix/);
    assert.throws(at("0001_.sql"), /has no description after prefix/);
    assert.deepEqual(loadMigrations(join(dir, "missing")).migrations, []);
    assert.throws(() => embedMigrations([], { table: "x; DROP TABLE y" }), /invalid migration table name/);
  });
});
